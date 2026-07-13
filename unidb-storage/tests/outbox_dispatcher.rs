//! Proves the outbox rides the item-20 dispatcher: a real
//! `unidb_dispatch::Dispatcher` subscribed to `objects` insert events drives the
//! `ConfirmSink`, which confirms the upload (`pending → ready`) as soon as the
//! bytes are present — retry/DLQ machinery reused from item 20.

mod common;

use std::sync::Arc;

use unidb_dispatch::{Dispatcher, Filter};
use unidb_storage::outbox::ConfirmSink;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn confirm_sink_confirms_pending_upload_via_dispatcher() {
    let h = common::harness(4).await;
    h.svc.create_bucket("b", None).await.unwrap();

    // Outbox: pending row + atomic insert event.
    let ticket = h.svc.begin_upload("b", "x.bin", None, None).await.unwrap();
    // Bytes land in the store (client PUT to the presigned URL).
    h.store.seed(&ticket.storage_key, b"payload");

    let sink = Arc::new(ConfirmSink::new(h.engine.clone(), h.store.clone()));
    let dispatcher = Dispatcher::builder(h.engine.clone(), "storage-confirm")
        .subscribe(Filter::table("objects").ops(["insert"]), sink)
        .build();

    // Drain: the insert event drives the sink, which confirms the upload.
    loop {
        if dispatcher.run_once().await.unwrap().polled == 0 {
            break;
        }
    }

    let row = h.svc.lookup("b", "x.bin").await.unwrap().unwrap();
    assert_eq!(
        row.status, "ready",
        "dispatcher-driven confirm flips to ready"
    );
}
