//! Storage service configuration, from env or built explicitly.
//!
//! `STORAGE_BACKEND` selects the object-store backend. Crucially it does **not**
//! select two different S3 code paths — `minio` and `s3` are the same wire impl
//! (`store::s3::S3ObjectStore`) with different defaults (endpoint + path-style +
//! creds). `memory` is the Docker-free in-process test double. See
//! `docs/design/storage_service.md` §2.

use std::time::Duration;

/// Which object-store backend the bytes live in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// In-process store — used by tests so `cargo test` needs no Docker.
    Memory,
    /// MinIO (dev): custom endpoint + path-style addressing, static env creds.
    Minio,
    /// AWS S3 (prod): regional endpoint, virtual-host style, static env creds.
    S3,
}

impl Backend {
    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "memory" | "mem" => Some(Backend::Memory),
            "minio" => Some(Backend::Minio),
            "s3" | "aws" => Some(Backend::S3),
            _ => None,
        }
    }
}

/// Complete storage-service configuration.
#[derive(Debug, Clone)]
pub struct StorageConfig {
    pub backend: Backend,
    /// Object-store endpoint URL (required for MinIO; optional for S3).
    pub endpoint: Option<String>,
    pub region: String,
    /// The single physical store bucket that holds every unidb bucket's objects
    /// (namespaced by a `"<bucket>/<key>"` storage key).
    pub bucket: String,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    /// MinIO needs path-style (`endpoint/bucket/key`); real S3 uses virtual-host.
    pub force_path_style: bool,
    /// Objects strictly smaller than this go inline as engine LOBs (ACID); larger
    /// objects go to the object store.
    pub inline_threshold: usize,
    /// Time-to-live for presigned PUT/GET URLs.
    pub presign_ttl: Duration,
    /// A `pending` row older than this whose bytes never arrived is compensated
    /// (`failed` + dead-letter) by the reconciler.
    pub pending_grace: Duration,
    /// A store object unreferenced by any live metadata row and older than this
    /// is swept as an orphan by the reconciler.
    pub orphan_grace: Duration,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            backend: Backend::Memory,
            endpoint: None,
            region: "us-east-1".to_string(),
            bucket: "unidb".to_string(),
            access_key: None,
            secret_key: None,
            force_path_style: false,
            inline_threshold: 1024 * 1024,
            presign_ttl: Duration::from_secs(900),
            pending_grace: Duration::from_secs(300),
            orphan_grace: Duration::from_secs(3600),
        }
    }
}

impl StorageConfig {
    /// A memory-backed config for tests: no store, no Docker.
    pub fn memory() -> Self {
        Self::default()
    }

    /// Build from environment variables. Unknown `STORAGE_BACKEND` is an error;
    /// everything else falls back to [`Default`].
    pub fn from_env() -> Result<Self, crate::StorageError> {
        let mut cfg = Self::default();
        if let Ok(b) = std::env::var("STORAGE_BACKEND") {
            cfg.backend = Backend::parse(&b).ok_or_else(|| {
                crate::StorageError::Config(format!("unknown STORAGE_BACKEND={b}"))
            })?;
        }
        // MinIO defaults to path-style; S3 defaults to virtual-host.
        cfg.force_path_style = matches!(cfg.backend, Backend::Minio);
        if let Ok(v) = std::env::var("STORAGE_ENDPOINT") {
            cfg.endpoint = Some(v);
        } else if let Ok(v) = std::env::var("STORAGE_S3_ENDPOINT") {
            cfg.endpoint = Some(v);
        }
        if let Ok(v) = std::env::var("STORAGE_REGION") {
            cfg.region = v;
        }
        if let Ok(v) = std::env::var("STORAGE_BUCKET") {
            cfg.bucket = v;
        }
        cfg.access_key = std::env::var("STORAGE_ACCESS_KEY").ok();
        cfg.secret_key = std::env::var("STORAGE_SECRET_KEY").ok();
        if let Ok(v) = std::env::var("STORAGE_FORCE_PATH_STYLE") {
            cfg.force_path_style = v == "1" || v.eq_ignore_ascii_case("true");
        }
        if let Some(n) = parse_env_usize("STORAGE_INLINE_THRESHOLD") {
            cfg.inline_threshold = n;
        }
        if let Some(n) = parse_env_u64("STORAGE_PRESIGN_TTL_SECS") {
            cfg.presign_ttl = Duration::from_secs(n);
        }
        if let Some(n) = parse_env_u64("STORAGE_PENDING_GRACE_SECS") {
            cfg.pending_grace = Duration::from_secs(n);
        }
        if let Some(n) = parse_env_u64("STORAGE_ORPHAN_GRACE_SECS") {
            cfg.orphan_grace = Duration::from_secs(n);
        }
        Ok(cfg)
    }
}

fn parse_env_usize(key: &str) -> Option<usize> {
    std::env::var(key).ok().and_then(|v| v.parse().ok())
}

fn parse_env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok().and_then(|v| v.parse().ok())
}
