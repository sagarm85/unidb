//! `unidb-server`: the optional REST/JWT/SSE/metrics server binary (M5).
//! Config comes from environment variables (no config file in v1):
//! - `UNIDB_DATA_DIR` (default `/tmp/unidb`): directory `Engine::open`s —
//!   holds `control`/`data.db`/`db.wal`, nothing else. The default lives under
//!   `/tmp` so a local/dev run never litters the working tree with data files
//!   (and they are never committed). `/tmp` is ephemeral across reboots — a
//!   persistent deployment must set this to a real data volume.
//! - `UNIDB_LOG_DIR` (default `<UNIDB_DATA_DIR>/logs`): directory for
//!   rolling daily log files (`unidb.log.YYYY-MM-DD`). Independently
//!   overridable so a deployment can put logs on a different volume than
//!   data (a common ops pattern — e.g. a smaller, faster disk for data,
//!   a larger/shared one for logs) while still defaulting to one
//!   self-contained folder for local/dev use.
//! - `UNIDB_LOG_RETAIN_DAYS` (default `7`): number of days of daily log files
//!   to keep at startup. Files matching `unidb.log.*` older than this are
//!   deleted before the new appender is created. Set to `0` to disable
//!   cleanup entirely.
//! - `UNIDB_PAGE_SIZE` (default `0`, meaning `Engine::open`'s own default).
//! - `UNIDB_BIND_ADDR` (default `127.0.0.1:8080`).
//! - `UNIDB_JWT_SECRET` (**required**): HMAC secret for HS256 JWT
//!   verification (`server::auth`). No default — refusing to start without
//!   an explicit secret is safer than silently running unauthenticated.
//!   Superseded for verification by `UNIDB_JWT_PUBLIC_KEY` when that is set
//!   (still required either way; see below).
//! - `UNIDB_JWT_SIGNING_KEY` (item 121 A5, optional): an explicit HS256
//!   signing secret that makes token issuance (`POST /auth/login`/`signup`/
//!   `refresh`) a first-class **production** capability, independent of the
//!   dev-only flag below. When set, this secret is used for BOTH verifying
//!   and issuing tokens (HS256 requires the same secret both ways) —
//!   superseding `UNIDB_JWT_SECRET` for that purpose. Takes precedence over
//!   `UNIDB_DEV_LOGIN` if both are set. Not compatible with
//!   `UNIDB_JWT_PUBLIC_KEY` (asymmetric verify mode disables local issuance
//!   entirely — see below).
//! - `UNIDB_DEV_LOGIN` (`1`/`true`, optional, pre-A5): back-compat alias for
//!   "enable issuance using `UNIDB_JWT_SECRET` as the signing secret." Kept
//!   working unchanged; `UNIDB_JWT_SIGNING_KEY` is the documented production
//!   path going forward.
//! - `UNIDB_JWT_PUBLIC_KEY` (item 121 A6, optional, PEM text): switches JWT
//!   verification to asymmetric RS256/ES256 — accepts tokens signed by an
//!   external IdP's matching private key. The concrete algorithm (RSA vs
//!   EC/P-256) is auto-detected from the key itself. Local token issuance is
//!   disabled outright in this mode (an HS256-signed local token could never
//!   verify against an asymmetric public key) — `UNIDB_JWT_SIGNING_KEY` and
//!   `UNIDB_DEV_LOGIN` are ignored (with a warning logged) if also set. The
//!   configured public key is served, JWK-encoded, at `GET
//!   /.well-known/jwks.json`.
//! - `UNIDB_AUTH_RATE_LIMIT` (item 121 I1, default `10`): max attempts per
//!   window for `POST /auth/login`/`signup`/`refresh` (the passwordless
//!   brute-force targets — see `server::rate_limit`'s module doc), keyed by
//!   client IP (peer address; `X-Forwarded-For` is deliberately not
//!   trusted — no reverse-proxy config exists to validate it). Set to `0`
//!   to disable rate limiting entirely.
//! - `UNIDB_AUTH_RATE_WINDOW_SECS` (default `60`): the window length backing
//!   `UNIDB_AUTH_RATE_LIMIT`.
//! - `UNIDB_OAUTH_<PROVIDER>_CLIENT_ID` / `_CLIENT_SECRET` / `_REDIRECT_URI`
//!   (item 128, Workstream D1, optional, per provider — `<PROVIDER>` is
//!   `GOOGLE` or `GITHUB`): activates `GET /auth/oauth/<provider>/authorize`
//!   and `.../callback`. A provider missing any of these three is simply not
//!   configured — its routes 404, same posture as `UNIDB_DEV_LOGIN`/
//!   `UNIDB_ALLOW_SIGNUP` being unset. Optional per-provider overrides
//!   `_AUTHORIZE_URL` / `_TOKEN_URL` / `_USERINFO_URL` / `_SCOPE` replace the
//!   real Google/GitHub endpoint defaults — mainly for pointing a test at a
//!   local mock provider. OAuth login still needs a signing key (see above)
//!   to hand back a session, same as signup/refresh.
//! - `UNIDB_CAPTCHA_PROTECT` (item 131, Workstream I2, default **empty =
//!   disabled**): comma-separated list of `POST /auth/login`/`/auth/signup`
//!   requiring a verified CAPTCHA `captcha_token` in the body. `UNIDB_
//!   CAPTCHA_PROVIDER` (default `turnstile`; also `hcaptcha`/`recaptcha`),
//!   `UNIDB_CAPTCHA_SECRET` (env fallback — resolved vault-first via the
//!   `captcha.secret` vault entry, item 129, exactly like OAuth's client
//!   secret), and `UNIDB_CAPTCHA_VERIFY_URL` (override the siteverify
//!   endpoint, mainly for pointing a test at a local mock) round out the
//!   config — see `server::captcha`'s module doc.
//!
//! Logging goes to **both** stdout (so `docker logs`/interactive/systemd
//! journal capture still works unchanged) and a rolling daily file under
//! `UNIDB_LOG_DIR` — unlike the embedded library's plain `unidb::
//! init_tracing()` (stdout-only, meant for an app that manages its own
//! logging), a long-running server process needs a persistent log a
//! restarted process or log-shipping agent can actually find. This is
//! deliberately implemented here rather than in `lib.rs` so the default,
//! non-`server` embedded build stays untouched.
//!
//! **Format (item 22, L1):** both sinks emit **JSON lines** by default —
//! `{ts, level, target, message, request_id, txn_id, …}` — the form any log
//! platform (CloudWatch, Datadog, Loki) ingests directly, and the form
//! `GET /logs` reads back (see `server::logs`, `ops_runbook.md`). Set
//! `UNIDB_LOG_FORMAT=text` for the older human-readable console format when
//! developing locally.
//!
//! Startup order: open the `Engine` via `EngineHandle::spawn` (synchronous,
//! surfaces any open/recovery failure immediately — see `engine_handle.rs`'s
//! module doc), build the router, bind, serve. `axum::serve`'s graceful
//! shutdown waits for in-flight requests to finish on `Ctrl-C`; once it
//! returns, every request-scoped `Arc<EngineHandle>` clone has already been
//! dropped, so the last reference (held here in `main`) drops naturally at
//! the end of this function, running `EngineHandle`'s `Drop` — which sends
//! `Shutdown` and joins the writer thread within its bounded timeout,
//! mirroring `IndexHandle`'s own shutdown-on-drop precedent.

use std::sync::Arc;

use axum_prometheus::PrometheusMetricLayer;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use unidb::server::{
    auth::JwtConfig, captcha::CaptchaConfig, email::EmailConfig, engine_handle::EngineHandle,
    oauth::OAuthConfig, router::build_router, AppState,
};

/// Delete `unidb.log.*` files in `log_dir` whose mtime is older than
/// `retain_days`. Called before the appender starts so we never delete the
/// file currently being written. No-ops silently on any I/O error (logging
/// is not initialized yet, so failures go to stderr).
fn cleanup_old_logs(log_dir: &std::path::Path, retain_days: u64) {
    if retain_days == 0 {
        return;
    }
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(retain_days * 86_400))
        .unwrap_or(std::time::UNIX_EPOCH);
    let Ok(entries) = std::fs::read_dir(log_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Match daily-rotated files: "unidb.log.YYYY-MM-DD"
        if !name.starts_with("unidb.log.") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if modified < cutoff {
            if let Err(e) = std::fs::remove_file(entry.path()) {
                eprintln!("warn: failed to remove old log {:?}: {e}", entry.path());
            }
        }
    }
}

/// Sets up dual stdout+file logging and returns the file appender's guard —
/// the caller must keep it alive for the process lifetime (dropping it
/// early silently stops flushing buffered log lines to the file, since
/// `tracing-appender`'s non-blocking writer flushes on `Drop`).
fn init_logging(log_dir: &str) -> tracing_appender::non_blocking::WorkerGuard {
    std::fs::create_dir_all(log_dir)
        .unwrap_or_else(|e| panic!("failed to create log directory {log_dir}: {e}"));
    let retain_days: u64 = std::env::var("UNIDB_LOG_RETAIN_DAYS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7);
    cleanup_old_logs(std::path::Path::new(log_dir), retain_days);
    let file_appender = tracing_appender::rolling::daily(log_dir, "unidb.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let env_filter =
        || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // JSON lines by default (item 22, L1) — the shipping contract for CW/Datadog
    // and the format `GET /logs` reads back. `UNIDB_LOG_FORMAT=text` restores the
    // human-readable console format for local development.
    let json = std::env::var("UNIDB_LOG_FORMAT")
        .map(|v| !v.eq_ignore_ascii_case("text"))
        .unwrap_or(true);

    let (stdout_layer, file_layer) = if json {
        (
            fmt::layer().json().boxed(),
            fmt::layer()
                .json()
                .with_writer(file_writer)
                .with_ansi(false)
                .boxed(),
        )
    } else {
        (
            fmt::layer().boxed(),
            fmt::layer()
                .with_writer(file_writer)
                .with_ansi(false)
                .boxed(),
        )
    };

    tracing_subscriber::registry()
        .with(env_filter())
        .with(stdout_layer)
        .with(file_layer)
        .init();
    guard
}

#[tokio::main]
async fn main() {
    let data_dir = std::env::var("UNIDB_DATA_DIR").unwrap_or_else(|_| "/tmp/unidb".to_string());
    let log_dir = std::env::var("UNIDB_LOG_DIR").unwrap_or_else(|_| format!("{data_dir}/logs"));
    // Held for the whole process lifetime — see `init_logging`'s doc comment.
    let _log_guard = init_logging(&log_dir);

    let page_size: u32 = std::env::var("UNIDB_PAGE_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let bind_addr =
        std::env::var("UNIDB_BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let jwt_secret = std::env::var("UNIDB_JWT_SECRET")
        .expect("UNIDB_JWT_SECRET must be set (verify-only JWT auth has no default secret)");

    let engine_handle = EngineHandle::spawn(std::path::Path::new(&data_dir), page_size)
        .unwrap_or_else(|e| panic!("failed to open unidb engine at {data_dir}: {e}"));

    // Item 34 Part A: optionally enable slow-query logging at startup.
    // 0 or absent = disabled (the engine default). Positive values enable it.
    if let Some(ms) = std::env::var("UNIDB_SLOW_QUERY_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&ms| ms > 0)
    {
        engine_handle
            .set_slow_query_threshold(ms)
            .await
            .unwrap_or_else(|e| panic!("failed to set slow_query_threshold: {e}"));
        tracing::info!(
            threshold_ms = ms,
            "slow-query logging enabled (UNIDB_SLOW_QUERY_MS)"
        );
    }

    // Builds the session/cursor registries and spawns the idle reaper (R1);
    // deadlines come from UNIDB_TXN_IDLE_TIMEOUT_SECS / _CURSOR_ (default 60).
    // Item 31: storage is None here (binary can't depend on unidb-storage
    // without a crate cycle). A custom embedding binary that depends on both
    // `unidb` and `unidb-storage` can call `.with_storage(Some(Arc::new(svc)))`.
    // All /storage/* routes return 503 when state.storage is None.
    // item 100 / item 121 A2: UNIDB_DEV_LOGIN=1 activates POST /auth/login
    // (real argon2id password verification since A2 — the flag now only
    // controls whether a signing key is available to issue tokens, not
    // whether the login is "real"). Milestone-18 "verify-only" is unchanged
    // when neither this flag nor UNIDB_JWT_SIGNING_KEY (below) is set.
    let dev_login = std::env::var("UNIDB_DEV_LOGIN")
        .ok()
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if dev_login {
        tracing::warn!(
            "UNIDB_DEV_LOGIN=1: POST /auth/login/signup/refresh can issue tokens using \
             UNIDB_JWT_SECRET as the signing key (pre-A5 back-compat path — prefer \
             UNIDB_JWT_SIGNING_KEY for production)"
        );
    }

    // item 121 A5: UNIDB_JWT_SIGNING_KEY makes issuance a first-class,
    // explicitly-configured production capability — independent of the
    // dev-only flag above (which stays working for back-compat). HS256
    // requires the same secret to sign and verify, so when this is set it
    // is used for BOTH: it supersedes UNIDB_JWT_SECRET as the effective
    // HS256 secret for tokens this server itself mints and verifies.
    let jwt_signing_key = std::env::var("UNIDB_JWT_SIGNING_KEY")
        .ok()
        .filter(|s| !s.is_empty());
    if let Some(ref _key) = jwt_signing_key {
        tracing::info!(
            "UNIDB_JWT_SIGNING_KEY is set: POST /auth/login/signup/refresh issuance is enabled \
             (item 121 A5 production issuer) using it as the effective HS256 secret for both \
             signing and verification, superseding UNIDB_JWT_SECRET for that purpose"
        );
    }

    // item 121 A6: UNIDB_JWT_PUBLIC_KEY switches verification to asymmetric
    // RS256/ES256 (accepts tokens signed by an external IdP's private key).
    // Local issuance is disabled outright in this mode — see the module doc.
    let jwt_public_key_pem = std::env::var("UNIDB_JWT_PUBLIC_KEY")
        .ok()
        .filter(|s| !s.is_empty());

    let jwt_config = if let Some(pem) = jwt_public_key_pem {
        if jwt_signing_key.is_some() || dev_login {
            tracing::warn!(
                "UNIDB_JWT_PUBLIC_KEY is set: asymmetric verify-only mode is active, so \
                 UNIDB_JWT_SIGNING_KEY / UNIDB_DEV_LOGIN are ignored — POST /auth/login, \
                 /auth/signup, and /auth/refresh cannot issue tokens (a locally HS256-signed \
                 token could never verify against a configured asymmetric public key)"
            );
        }
        tracing::info!(
            "UNIDB_JWT_PUBLIC_KEY is set: JWT verification is asymmetric (RS256/ES256, \
             auto-detected); the public key is served at GET /.well-known/jwks.json"
        );
        JwtConfig::from_asymmetric_public_pem(pem.as_bytes())
            .unwrap_or_else(|e| panic!("invalid UNIDB_JWT_PUBLIC_KEY: {e}"))
    } else if let Some(ref signing_key) = jwt_signing_key {
        JwtConfig::with_signing_key(signing_key)
    } else if dev_login {
        JwtConfig::with_dev_login(&jwt_secret)
    } else {
        JwtConfig::new(&jwt_secret)
    };

    // item 121, A3: UNIDB_ALLOW_SIGNUP=1 activates POST /auth/signup
    // (default off — opt-in, not open by default). Signup still needs a
    // token-issuing key to hand back access/refresh tokens, so it is only
    // useful paired with UNIDB_JWT_SIGNING_KEY (A5) or UNIDB_DEV_LOGIN=1.
    let allow_signup = std::env::var("UNIDB_ALLOW_SIGNUP")
        .ok()
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if allow_signup {
        tracing::warn!(
            "UNIDB_ALLOW_SIGNUP=1: POST /auth/signup is enabled (self-service account \
             creation — make sure this is the posture you want in production)"
        );
    }

    // item 128: OAuth social-login providers, read from
    // UNIDB_OAUTH_<PROVIDER>_* — see this file's module doc. Logged (without
    // secrets) so a deployment can see at a glance which providers are live.
    let oauth_config = OAuthConfig::from_env();
    for name in ["google", "github"] {
        if oauth_config.get(name).is_some() {
            tracing::info!(provider = name, "OAuth social login provider configured");
        }
    }

    // item 131 (Workstream I2): CAPTCHA/bot-protection gate, disabled by
    // default (UNIDB_CAPTCHA_PROTECT unset = empty = protects nothing).
    let captcha_config = CaptchaConfig::from_env();
    if captcha_config.is_enabled() {
        tracing::info!("CAPTCHA verification enabled (UNIDB_CAPTCHA_PROTECT set)");
    }

    // item 138: outbound email transport for the auth email flows
    // (password reset, magic link). Defaults to the dev-inbox log
    // transport (no config needed); `UNIDB_EMAIL_TRANSPORT=smtp` with
    // missing required config fails clearly here at startup, same posture
    // as a malformed `UNIDB_MASTER_KEY`.
    let email_config =
        EmailConfig::from_env().unwrap_or_else(|e| panic!("invalid email transport config: {e}"));
    tracing::info!(
        transport = if matches!(
            std::env::var("UNIDB_EMAIL_TRANSPORT").as_deref(),
            Ok("smtp")
        ) {
            "smtp"
        } else {
            "log"
        },
        "email transport configured"
    );

    let state = {
        let mut s = AppState::new(Arc::new(engine_handle))
            .with_log_dir(std::path::PathBuf::from(&log_dir))
            .with_email(email_config);
        // `AppState::with_dev_login` just stores "the config that can mint
        // tokens" — checking `encoding_key.is_some()` (rather than
        // re-deriving the dev_login/signing_key/public_key branching here)
        // is the single source of truth: it's `None` in every disabled case,
        // including asymmetric-verify mode, where issuance is unconditionally
        // off regardless of what UNIDB_DEV_LOGIN/UNIDB_JWT_SIGNING_KEY say.
        if jwt_config.encoding_key.is_some() {
            s = s.with_dev_login(jwt_config.clone());
        }
        s.with_allow_signup(allow_signup)
            .with_oauth(oauth_config)
            .with_captcha(captcha_config)
    };
    let (prometheus_layer, metric_handle) = PrometheusMetricLayer::pair();

    let router = build_router(state, jwt_config, prometheus_layer, metric_handle);

    // P6.f: serve HTTPS directly when TLS is configured; otherwise plain HTTP.
    if let Some((cert, key)) = unidb::server::tls::tls_paths_from_env() {
        unidb::server::tls::install_crypto_provider();
        let config = unidb::server::tls::load_rustls_config(cert.as_ref(), key.as_ref())
            .await
            .unwrap_or_else(|e| panic!("failed to load TLS cert/key: {e}"));
        let addr: std::net::SocketAddr = bind_addr
            .parse()
            .unwrap_or_else(|e| panic!("invalid UNIDB_BIND_ADDR {bind_addr}: {e}"));
        tracing::info!(addr = %bind_addr, data_dir = %data_dir, log_dir = %log_dir, tls = true, "unidb-server listening (HTTPS)");
        // `into_make_service_with_connect_info` (rather than plain
        // `into_make_service`) so `ConnectInfo<SocketAddr>` resolves inside
        // `server::rate_limit::rate_limit_auth` (item 121 I1) — the peer
        // address is the rate limiter's client-IP key.
        axum_server::bind_rustls(addr, config)
            .serve(router.into_make_service_with_connect_info::<std::net::SocketAddr>())
            .await
            .expect("server error");
    } else {
        let listener = tokio::net::TcpListener::bind(&bind_addr)
            .await
            .unwrap_or_else(|e| panic!("failed to bind {bind_addr}: {e}"));
        tracing::info!(addr = %bind_addr, data_dir = %data_dir, log_dir = %log_dir, tls = false, "unidb-server listening (HTTP)");
        // Same connect-info wiring as the TLS branch above.
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
    }

    tracing::info!("unidb-server shut down cleanly");
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install Ctrl-C handler");
    tracing::info!("shutdown signal received, draining in-flight requests");
}
