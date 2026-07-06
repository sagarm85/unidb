//! Optional REST/JWT/SSE/metrics server (M5), gated behind the `server`
//! Cargo feature so a default `cargo build`/`cargo test` of the embedded
//! crate never depends on tokio/axum/etc. — see `lib.rs`'s crate doc and
//! `CLAUDE.md`'s "tokio (M5 server only — the engine stays sync)" note.
//!
//! **The core architectural decision: async HTTP handlers never touch
//! `Engine` directly.** One dedicated OS thread ([`engine_handle::spawn`])
//! owns the `Engine` for its entire life, exactly as `index_worker.rs`'s
//! background thread owns its secondary-index structures. Handlers send an
//! [`engine_handle::EngineRequest`] over an `mpsc` channel and `.await` a
//! per-request `oneshot` reply. This was chosen over a shared
//! `Mutex<Engine>` deliberately: a mutex held across an `.await` point is a
//! well-known anti-pattern, and even held only for a call's duration it
//! reintroduces exactly the kind of incidental cross-thread mutable access
//! to `Engine` that its single-threaded-by-design shape was built to avoid.
//! The writer thread preserves that invariant instead of asking every
//! future call site to remember a new locking discipline forever.
//!
//! Submodules: [`engine_handle`] (the writer-thread bridge), [`error`]
//! (`DbError` → HTTP status mapping). Later checkpoints add `handlers`,
//! `router`, `dto` (M5.b), `auth`, `sse`, `metrics` (M5.c).

pub mod engine_handle;
pub mod error;
