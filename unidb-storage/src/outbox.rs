//! [`ConfirmSink`] — the optional item-20 Dispatcher fast-path for confirming
//! uploads.
//!
//! This proves the outbox end-to-end on the item-20 contract: subscribe a
//! [`unidb_dispatch::Dispatcher`] to `objects` insert events (each committed
//! atomically with its metadata row) and let this [`Sink`] confirm uploads as
//! soon as the bytes land — `pending → ready` — with fan-out/retry/DLQ for free.
//!
//! It is a **fast path, not the authority**: if the bytes have not arrived yet
//! it returns `Err` (a bounded number of quick retries), and it never flips a
//! row to `failed`. The wall-clock grace decision (compensate to `failed`) stays
//! with the [`Reconciler`](crate::Reconciler), because the Dispatcher's retry is
//! a millisecond in-cycle loop, not an upload grace timer (see
//! `docs/design/storage_service.md` §4.2).
//!
//! ## Wiring
//!
//! ```no_run
//! # use std::sync::Arc;
//! # async fn wire(engine: Arc<unidb::Engine>, store: Arc<dyn unidb_storage::ObjectStore>) {
//! use unidb_dispatch::{Dispatcher, Filter};
//! use unidb_storage::outbox::ConfirmSink;
//!
//! let sink = Arc::new(ConfirmSink::new(engine.clone(), store));
//! let dispatcher = Dispatcher::builder(engine, "storage-confirm")
//!     .subscribe(Filter::table("objects").ops(["insert"]), sink)
//!     .build();
//! // dispatcher.run(shutdown).await;  // or run_once() in a test
//! # }
//! ```

use std::sync::Arc;

use async_trait::async_trait;
use unidb::queue::Event;
use unidb::Engine;
use unidb_dispatch::{Sink, SinkError};

use crate::metadata;
use crate::service::storage_key;
use crate::store::ObjectStore;

/// A Dispatcher sink that confirms pending uploads by HEAD-ing the store.
pub struct ConfirmSink {
    name: String,
    engine: Arc<Engine>,
    store: Arc<dyn ObjectStore>,
}

impl ConfirmSink {
    pub fn new(engine: Arc<Engine>, store: Arc<dyn ObjectStore>) -> Self {
        Self {
            name: "storage-confirm".to_string(),
            engine,
            store,
        }
    }
}

fn str_field<'a>(payload: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(|v| v.as_str())
}

#[async_trait]
impl Sink for ConfirmSink {
    fn name(&self) -> &str {
        &self.name
    }

    async fn deliver(&self, event: &Event) -> Result<(), SinkError> {
        // Only act on freshly-inserted pending S3 objects.
        if event.table_name != metadata::OBJECTS_TABLE {
            return Ok(());
        }
        let payload = &event.payload;
        if str_field(payload, "status") != Some(metadata::status::PENDING) {
            return Ok(());
        }
        let bucket = str_field(payload, "bucket")
            .ok_or_else(|| SinkError::new("event payload missing 'bucket'"))?;
        let object_key = str_field(payload, "object_key")
            .ok_or_else(|| SinkError::new("event payload missing 'object_key'"))?;
        let skey = storage_key(bucket, object_key);

        // Bytes present yet?
        let meta = self
            .store
            .head(&skey)
            .await
            .map_err(|e| SinkError::new(format!("head {skey}: {e}")))?;
        let Some(meta) = meta else {
            // Not uploaded yet — a transient miss. Let the dispatcher retry; the
            // reconciler is the authority that eventually compensates.
            return Err(SinkError::new(format!("bytes not yet present for {skey}")));
        };

        let engine = self.engine.clone();
        let (b, k) = (bucket.to_string(), object_key.to_string());
        let etag = meta.etag.clone();
        let size = meta.size as i64;
        tokio::task::spawn_blocking(move || {
            let xid = engine.begin()?;
            match metadata::mark_ready(&engine, xid, &b, &k, etag.as_deref(), size) {
                Ok(()) => engine.commit(xid),
                Err(e) => {
                    let _ = engine.abort(xid);
                    Err(e)
                }
            }
        })
        .await
        .map_err(|_| SinkError::new("confirm join failed"))?
        .map_err(|e| SinkError::new(format!("mark ready: {e}")))?;
        Ok(())
    }
}
