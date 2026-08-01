//! Outbound email transport + templates for auth email flows (item 138,
//! the Wave-1 lead item of the free [Supabase parity
//! roadmap](../../../docs/backlog/137_supabase_parity_free_roadmap.md)).
//! Mirrors `server::oauth`'s shape closely: outbound network from the
//! server, config from env with a vault-first secret
//! ([`resolve_smtp_password`], exactly like `oauth::resolve_client_secret`),
//! and a local mock/no-network path for tests ([`LogTransport`]).
//!
//! **No `users.email` column (a deliberate, disclosed scope decision).**
//! `src/authz/mod.rs`'s `RoleStore` identifies accounts purely by
//! `username` — there is no separate email address on file for any user.
//! Rather than add a new persisted column in this PR (out of scope per the
//! item-138 backlog doc's non-goals — "email confirmation on signup"/
//! "email-change flow" land later, once a real `email` column exists), the
//! recovery/magic-link flows here treat the `email` field a caller supplies
//! to `POST /auth/recover`/`POST /auth/magiclink` as **the username to look
//! up directly** — i.e. this milestone assumes accounts are provisioned
//! with their email address as their username (a common, supported
//! convention; nothing stops a deployment from doing this today via `POST
//! /auth/signup`). `docs/backlog/137_supabase_parity_free_roadmap.md`
//! tracks a real `users.email` column as a fast follow-up so this
//! assumption can be dropped without an API shape change (the request body
//! stays `{ "email": "..." }` either way).
//!
//! **Transport selection** (`UNIDB_EMAIL_TRANSPORT=log|smtp`, default
//! `log`):
//! - [`LogTransport`] — writes the fully-rendered email to a dev-inbox file
//!   (`UNIDB_EMAIL_DEV_FILE`, default `<data dir>/email-dev-inbox.jsonl`)
//!   as one JSON line per send, plus a `tracing::info!` line. Never touches
//!   the network. This is the default so dev/tests need no mail server —
//!   mirrors Supabase's Inbucket/Mailpit story.
//! - [`SmtpTransport`] — real delivery via `lettre`
//!   (`AsyncSmtpTransport<Tokio1Executor>`). Config from env:
//!   `UNIDB_SMTP_HOST`/`_PORT`/`_USERNAME`/`_FROM`, `UNIDB_SMTP_TLS_MODE`
//!   (`tls` (default, implicit TLS/SMTPS) | `starttls` | `none`). The
//!   password is resolved **vault-first**: a vault secret named
//!   [`SMTP_PASSWORD_SECRET_NAME`] wins over `UNIDB_SMTP_PASSWORD` — same
//!   order/fail-closed contract as [`crate::server::oauth::
//!   resolve_client_secret`]. `UNIDB_EMAIL_TRANSPORT=smtp` with no
//!   `UNIDB_SMTP_HOST`/`UNIDB_SMTP_FROM` fails clearly at
//!   [`EmailConfig::from_env`] time (startup), same posture as an OAuth
//!   provider missing required config.
//!
//! **Templates.** [`EmailTemplate`] is a named subject + text body +
//! optional HTML body with `{{link}}`/`{{code}}`/`{{user}}`/`{{site_url}}`
//! substitution ([`render_template`]). Built-in defaults exist for
//! `"recovery"` and `"magiclink"`; `UNIDB_EMAIL_TEMPLATES_DIR` overrides
//! either (or both) by supplying `<name>.txt` (first line = subject, the
//! rest = the text body) and an optional `<name>.html`. Substituted values
//! are HTML-escaped before being spliced into the HTML body ([`html_escape`])
//! — the template markup itself is trusted (operator-supplied), the
//! *values* (username, link, TTL-bound token) are not.
//!
//! **Security.** Every token this module ever mints
//! ([`crate::authz::RoleStore::create_recovery_token`]/
//! [`create_magiclink_token`]) is single-use, short-TTL, and hash-only
//! persisted (see that module's doc comments) — this module only ever
//! receives an already-minted raw token to embed in a link, never mints one
//! itself. The SMTP password and every rendered email body are kept out of
//! `Debug`/`tracing` at `info` level or above (see [`EmailMessage`]'s manual
//! `Debug` and [`SmtpConfig`]'s).

use std::path::PathBuf;

use crate::error::{DbError, Result};
use crate::server::engine_handle::EngineHandle;

/// Vault secret name for the SMTP password — mirrors `oauth::
/// oauth_secret_name`/`captcha::CAPTCHA_SECRET_NAME`'s convention. `pub` so
/// both this module and an operator running `unidb-vault set` know the
/// exact literal.
pub const SMTP_PASSWORD_SECRET_NAME: &str = "smtp.password";

// ── EmailMessage + EmailTransport ───────────────────────────────────────

/// One outbound email, fully rendered (subject/body already substituted) —
/// the shape both [`EmailTransport`] impls send.
#[derive(Clone)]
pub struct EmailMessage {
    pub to: String,
    pub from: String,
    pub subject: String,
    pub text_body: String,
    pub html_body: Option<String>,
}

/// Manual `Debug`: redacts `subject`/`text_body`/`html_body` — a rendered
/// auth email embeds a live, single-use recovery/magic-link token in its
/// link, so the body is exactly as sensitive as the token itself and must
/// never appear in a `{:?}`/`tracing::debug!` (CLAUDE.md's "nothing secret
/// in logs/Debug" rule, same posture as [`crate::server::dto::
/// AuthLoginResponse`]'s bearer-token redaction). `to`/`from` are shown —
/// an email address is the same non-secret identifier a `username` already
/// is elsewhere in this codebase (see this module's doc comment on the
/// email-as-username design choice).
impl std::fmt::Debug for EmailMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmailMessage")
            .field("to", &self.to)
            .field("from", &self.from)
            .field("subject", &"<redacted>")
            .field("text_body", &"<redacted>")
            .field("html_body", &self.html_body.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Provider-agnostic outbound email transport (item 138). Implemented by
/// [`SmtpTransport`] (real delivery) and [`LogTransport`] (dev-inbox file,
/// no network) — selection happens once, at startup, via
/// [`EmailConfig::from_env`].
pub trait EmailTransport: Send + Sync {
    /// Send `msg`. `Err` for a transport-layer failure (SMTP connect/auth/
    /// send failure, or a filesystem error writing the dev-inbox file) —
    /// callers (`handlers.rs`) log-and-swallow this for the no-account-
    /// enumeration routes (`POST /auth/recover`/`/auth/magiclink` always
    /// return `200` regardless of delivery outcome — see their doc
    /// comments), never surface it to the client as a distinguishable
    /// error.
    fn send(&self, msg: &EmailMessage) -> impl std::future::Future<Output = Result<()>> + Send;
}

// ── LogTransport (default; no network) ──────────────────────────────────

/// Writes the fully-rendered email to a dev-inbox file (one JSON line per
/// send) and to `tracing`, instead of actually sending it. The default
/// transport ([`EmailConfig::from_env`]) — no mail server needed for local
/// dev or tests. `tests/item138_email_auth.rs` reads this file to recover
/// the token embedded in a captured email's link.
#[derive(Clone)]
pub struct LogTransport {
    path: PathBuf,
}

impl LogTransport {
    /// Point the dev inbox at an explicit file path — used by tests so each
    /// test gets its own isolated inbox file, and by [`Self::from_env`].
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// `UNIDB_EMAIL_DEV_FILE`, defaulting to `<UNIDB_DATA_DIR or
    /// /tmp/unidb>/email-dev-inbox.jsonl` — mirrors `unidb-server.rs`'s own
    /// `UNIDB_LOG_DIR`-vs-`UNIDB_DATA_DIR` default resolution.
    pub fn from_env() -> Self {
        let path = std::env::var("UNIDB_EMAIL_DEV_FILE")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(default_dev_inbox_path);
        Self::new(path)
    }

    /// The dev-inbox file path this transport writes to — exposed so tests
    /// can construct it once and hand it to both [`LogTransport::new`] and
    /// their own file-reading assertions.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

fn default_dev_inbox_path() -> PathBuf {
    let data_dir = std::env::var("UNIDB_DATA_DIR").unwrap_or_else(|_| "/tmp/unidb".to_string());
    PathBuf::from(data_dir).join("email-dev-inbox.jsonl")
}

impl EmailTransport for LogTransport {
    async fn send(&self, msg: &EmailMessage) -> Result<()> {
        // Never log the subject/body at info (they carry a live token) —
        // this line is deliberately just the envelope, same posture as
        // every other auth-flow `tracing::info!` in this codebase (e.g.
        // `RoleStore::set_password`'s "password credential set").
        tracing::info!(
            to = %msg.to,
            from = %msg.from,
            "email captured by the dev-inbox log transport (UNIDB_EMAIL_TRANSPORT=log — not actually sent)"
        );
        let record = serde_json::json!({
            "to": msg.to,
            "from": msg.from,
            "subject": msg.subject,
            "text_body": msg.text_body,
            "html_body": msg.html_body,
        });
        let mut line = serde_json::to_string(&record)
            .map_err(|e| DbError::Authz(format!("email dev-inbox: serialize failed: {e}")))?;
        line.push('\n');
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(line.as_bytes())?;
        Ok(())
    }
}

// ── SmtpTransport (real delivery via `lettre`) ──────────────────────────

/// `UNIDB_SMTP_TLS_MODE` (default [`SmtpTlsMode::Tls`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmtpTlsMode {
    /// Implicit TLS (SMTPS) — the connection is encrypted from the first
    /// byte. `lettre`'s `relay()` default; the safe default here too.
    Tls,
    /// Opportunistic upgrade to TLS via the `STARTTLS` command on an
    /// initially-plaintext connection — the common submission-port-587
    /// posture.
    Starttls,
    /// No encryption at all. **Only** appropriate for a trusted local relay
    /// (e.g. a self-hosted Mailpit/Mailhog dev box) — never for a remote
    /// server.
    None,
}

impl SmtpTlsMode {
    fn from_env() -> Self {
        match std::env::var("UNIDB_SMTP_TLS_MODE")
            .ok()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "starttls" => SmtpTlsMode::Starttls,
            "none" => SmtpTlsMode::None,
            _ => SmtpTlsMode::Tls,
        }
    }
}

/// Non-secret SMTP connection config, captured once at startup (item 138) —
/// mirrors `oauth::OAuthProviderConfig`: everything except the password,
/// which is resolved vault-first at send time (see [`resolve_smtp_password`])
/// rather than cached here.
#[derive(Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    /// Raw `UNIDB_SMTP_PASSWORD` value (may be empty — [`resolve_smtp_password`]
    /// checks the vault first, then falls back to this).
    env_password: String,
    pub tls_mode: SmtpTlsMode,
}

/// Manual `Debug`: redacts `env_password` — never printed/logged, same
/// posture as `oauth::OAuthProviderConfig`'s `client_secret` redaction.
impl std::fmt::Debug for SmtpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("env_password", &"<redacted>")
            .field("tls_mode", &self.tls_mode)
            .finish()
    }
}

impl SmtpConfig {
    /// Construct directly — used by tests to exercise config-shape logic
    /// (never a real `.send()`) without touching process-global env vars.
    pub fn new(
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
        env_password: impl Into<String>,
        tls_mode: SmtpTlsMode,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            username: username.into(),
            env_password: env_password.into(),
            tls_mode,
        }
    }
}

/// Resolve the *effective* SMTP password (item 138): vault first
/// ([`SMTP_PASSWORD_SECRET_NAME`]), falling back to `cfg`'s env-sourced
/// value — identical order/semantics to
/// [`crate::server::oauth::resolve_client_secret`]. `Err` (never a silent
/// empty/placeholder password) when the vault lookup itself fails or the
/// password is in neither place.
pub async fn resolve_smtp_password(engine: &EngineHandle, cfg: &SmtpConfig) -> Result<String> {
    match engine
        .get_secret(SMTP_PASSWORD_SECRET_NAME.to_string())
        .await
    {
        Ok(Some(secret)) => Ok(secret),
        Ok(None) => {
            if cfg.env_password.is_empty() {
                Err(DbError::Authz(format!(
                    "no SMTP password configured — set it via `unidb-vault set \
                     {SMTP_PASSWORD_SECRET_NAME}` or UNIDB_SMTP_PASSWORD"
                )))
            } else {
                Ok(cfg.env_password.clone())
            }
        }
        Err(e) => Err(DbError::Authz(format!(
            "vault error resolving SMTP password: {e}"
        ))),
    }
}

/// Real SMTP delivery via `lettre`'s async Tokio transport. Built fresh
/// per-send with the vault-or-env-resolved password (see
/// [`resolve_smtp_password`]) rather than cached on [`EmailConfig`] — SMTP
/// sends from this codebase are rare (auth-flow emails, not a hot path), so
/// the extra connection-pool-warmup cost of not reusing a client is a
/// non-issue, and it means a rotated vault password takes effect on the
/// very next send with no cache-invalidation logic needed.
pub struct SmtpTransport {
    host: String,
    port: u16,
    username: String,
    password: String,
    tls_mode: SmtpTlsMode,
}

/// Manual `Debug`: redacts `password` — never printed/logged.
impl std::fmt::Debug for SmtpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpTransport")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("tls_mode", &self.tls_mode)
            .finish()
    }
}

impl SmtpTransport {
    pub fn new(cfg: &SmtpConfig, password: String) -> Self {
        Self {
            host: cfg.host.clone(),
            port: cfg.port,
            username: cfg.username.clone(),
            password,
            tls_mode: cfg.tls_mode,
        }
    }
}

/// Build a `lettre::Message` from a rendered [`EmailMessage`]: plain text
/// only if no HTML body was rendered, `multipart/alternative` (text + HTML)
/// otherwise — the conventional MIME shape every mail client expects.
fn build_lettre_message(msg: &EmailMessage) -> Result<lettre::Message> {
    use lettre::message::{header::ContentType, Mailbox, MultiPart};

    let from: Mailbox = msg
        .from
        .parse()
        .map_err(|e| DbError::Authz(format!("invalid email 'from' address: {e}")))?;
    let to: Mailbox = msg
        .to
        .parse()
        .map_err(|e| DbError::Authz(format!("invalid email 'to' address: {e}")))?;
    let builder = lettre::Message::builder()
        .from(from)
        .to(to)
        .subject(msg.subject.clone());

    let built = if let Some(html) = &msg.html_body {
        builder.multipart(MultiPart::alternative_plain_html(
            msg.text_body.clone(),
            html.clone(),
        ))
    } else {
        builder
            .header(ContentType::TEXT_PLAIN)
            .body(msg.text_body.clone())
    };
    built.map_err(|e| DbError::Authz(format!("failed to build email message: {e}")))
}

impl EmailTransport for SmtpTransport {
    async fn send(&self, msg: &EmailMessage) -> Result<()> {
        use lettre::{
            transport::smtp::{authentication::Credentials, client::Tls},
            AsyncSmtpTransport, AsyncTransport, Tokio1Executor,
        };

        let email = build_lettre_message(msg)?;

        let builder = match self.tls_mode {
            SmtpTlsMode::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&self.host),
            SmtpTlsMode::Starttls => {
                AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&self.host)
            }
            SmtpTlsMode::None => Ok(AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(
                &self.host,
            )),
        }
        .map_err(|e| DbError::Authz(format!("smtp transport config error: {e}")))?;

        let mut builder = builder.port(self.port);
        if self.tls_mode == SmtpTlsMode::None {
            builder = builder.tls(Tls::None);
        }
        if !self.username.is_empty() {
            builder = builder.credentials(Credentials::new(
                self.username.clone(),
                self.password.clone(),
            ));
        }
        let mailer: AsyncSmtpTransport<Tokio1Executor> = builder.build();

        mailer
            .send(email)
            .await
            .map_err(|e| DbError::Authz(format!("smtp send failed: {e}")))?;
        Ok(())
    }
}

// ── Templates ────────────────────────────────────────────────────────────

/// A named email template: subject + text body + optional HTML body, each
/// still carrying unsubstituted `{{link}}`/`{{code}}`/`{{user}}`/
/// `{{site_url}}` placeholders — see [`render_template`].
#[derive(Clone)]
pub struct EmailTemplate {
    pub subject: String,
    pub text_body: String,
    pub html_body: Option<String>,
}

/// The substitution variables [`render_template`] recognizes. Any field left
/// `None` substitutes to an empty string — a template author who references
/// a variable the caller didn't supply gets an empty splice, never a panic
/// or a literal `{{...}}` left in the output.
#[derive(Default)]
pub struct TemplateVars<'a> {
    pub link: Option<&'a str>,
    pub code: Option<&'a str>,
    pub user: Option<&'a str>,
    pub site_url: Option<&'a str>,
}

/// HTML-escape the five characters that matter for safely splicing
/// arbitrary text into HTML markup (`&`/`<`/`>`/`"`/`'`). Hand-rolled rather
/// than pulling in an HTML-escaping crate — this is a 5-character allowlist
/// substitution, not general HTML parsing (mirrors CLAUDE.md's "hand-roll
/// the small thing" call already made for TOTP's base32 in `authz/mod.rs`).
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Replace every `{{link}}`/`{{code}}`/`{{user}}`/`{{site_url}}` occurrence
/// in `template` with the corresponding value from `vars` — `escape_html`
/// controls whether each *value* is HTML-escaped before splicing (the
/// template markup itself is never escaped/altered; only the substituted
/// values are, and only when rendering an HTML body).
fn substitute(template: &str, vars: &TemplateVars<'_>, escape_html: bool) -> String {
    let pairs: [(&str, Option<&str>); 4] = [
        ("{{link}}", vars.link),
        ("{{code}}", vars.code),
        ("{{user}}", vars.user),
        ("{{site_url}}", vars.site_url),
    ];
    let mut out = template.to_string();
    for (token, value) in pairs {
        let raw = value.unwrap_or("");
        let replacement = if escape_html {
            html_escape(raw)
        } else {
            raw.to_string()
        };
        out = out.replace(token, &replacement);
    }
    out
}

/// Render `template` against `vars`, producing `(subject, text_body,
/// html_body)` ready to hand to an [`EmailMessage`]. The HTML body (if any)
/// has its substituted values HTML-escaped ([`html_escape`]); the subject
/// and text body do not need escaping (plain text has no markup to break
/// out of).
pub fn render_template(
    template: &EmailTemplate,
    vars: &TemplateVars<'_>,
) -> (String, String, Option<String>) {
    let subject = substitute(&template.subject, vars, false);
    let text_body = substitute(&template.text_body, vars, false);
    let html_body = template
        .html_body
        .as_ref()
        .map(|h| substitute(h, vars, true));
    (subject, text_body, html_body)
}

const DEFAULT_RECOVERY_SUBJECT: &str = "Reset your unidb password";
const DEFAULT_RECOVERY_TEXT: &str = "Hi {{user}},\n\n\
A password reset was requested for your account. If this was you, open the \
link below to choose a new password. This link expires in an hour and can \
only be used once:\n\n\
{{link}}\n\n\
If you didn't request this, you can safely ignore this email — your \
password will not change.\n";
const DEFAULT_RECOVERY_HTML: &str = "<p>Hi {{user}},</p>\
<p>A password reset was requested for your account. If this was you, click \
the link below to choose a new password. This link expires in an hour and \
can only be used once.</p>\
<p><a href=\"{{link}}\">Reset your password</a></p>\
<p>If you didn't request this, you can safely ignore this email — your \
password will not change.</p>";

const DEFAULT_MAGICLINK_SUBJECT: &str = "Your unidb sign-in link";
const DEFAULT_MAGICLINK_TEXT: &str = "Hi {{user}},\n\n\
Open the link below to sign in. This link expires in 15 minutes and can \
only be used once:\n\n\
{{link}}\n\n\
If you didn't request this, you can safely ignore this email.\n";
const DEFAULT_MAGICLINK_HTML: &str = "<p>Hi {{user}},</p>\
<p>Click the link below to sign in. This link expires in 15 minutes and can \
only be used once.</p>\
<p><a href=\"{{link}}\">Sign in to unidb</a></p>\
<p>If you didn't request this, you can safely ignore this email.</p>";

fn default_template(name: &str) -> EmailTemplate {
    match name {
        "recovery" => EmailTemplate {
            subject: DEFAULT_RECOVERY_SUBJECT.to_string(),
            text_body: DEFAULT_RECOVERY_TEXT.to_string(),
            html_body: Some(DEFAULT_RECOVERY_HTML.to_string()),
        },
        "magiclink" => EmailTemplate {
            subject: DEFAULT_MAGICLINK_SUBJECT.to_string(),
            text_body: DEFAULT_MAGICLINK_TEXT.to_string(),
            html_body: Some(DEFAULT_MAGICLINK_HTML.to_string()),
        },
        other => EmailTemplate {
            subject: format!("unidb: {other}"),
            text_body: "{{link}}".to_string(),
            html_body: None,
        },
    }
}

/// Load an override from `dir` for template `name`: `<name>.txt` (required —
/// first line is the subject, the remainder is the text body) plus an
/// optional `<name>.html`. `None` if `<name>.txt` doesn't exist or can't be
/// read — the caller ([`load_template`]) falls back to the built-in default
/// in that case, so a templates dir that only overrides `"recovery"` still
/// leaves `"magiclink"` (or any other name) working.
fn load_template_from_dir(dir: &std::path::Path, name: &str) -> Option<EmailTemplate> {
    let text_file = std::fs::read_to_string(dir.join(format!("{name}.txt"))).ok()?;
    let mut lines = text_file.splitn(2, '\n');
    let subject = lines.next().unwrap_or_default().trim().to_string();
    let text_body = lines.next().unwrap_or_default().to_string();
    let html_body = std::fs::read_to_string(dir.join(format!("{name}.html"))).ok();
    Some(EmailTemplate {
        subject,
        text_body,
        html_body,
    })
}

/// Resolve template `name` — `UNIDB_EMAIL_TEMPLATES_DIR` override first (if
/// set and it has a matching `<name>.txt`), else the built-in default. Never
/// fails: an unrecognized `name` with no override gets a minimal generic
/// template ([`default_template`]) rather than an error, since template
/// rendering must never be the reason an auth-email send fails.
pub fn load_template(name: &str) -> EmailTemplate {
    if let Ok(dir) = std::env::var("UNIDB_EMAIL_TEMPLATES_DIR") {
        if !dir.is_empty() {
            if let Some(t) = load_template_from_dir(std::path::Path::new(&dir), name) {
                return t;
            }
        }
    }
    default_template(name)
}

// ── EmailConfig (selection, held on AppState) ───────────────────────────

/// Which concrete transport is active — mirrors `oauth::OAuthConfig`/
/// `captcha::CaptchaConfig`: a concrete config value on [`crate::server::
/// AppState`], not a `Box<dyn EmailTransport>` (native `async fn` trait
/// methods aren't `dyn`-safe without extra boxing machinery this crate
/// doesn't otherwise need — see the module doc; both variants still
/// implement [`EmailTransport`] directly for their own unit tests).
#[derive(Clone, Debug)]
enum EmailTransportSelection {
    Log(LogTransport),
    Smtp(SmtpConfig),
}

/// [`LogTransport`] carries only a file path — nothing secret — so a plain
/// derive is fine; [`SmtpConfig`] already has its own password-redacting
/// manual `Debug`.
impl std::fmt::Debug for LogTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogTransport")
            .field("path", &self.path)
            .finish()
    }
}

/// Outbound-email configuration for the auth email flows (item 138), held
/// on [`crate::server::AppState`]. Selects + dispatches to the configured
/// transport and owns the `{{site_url}}` base used to build recovery/
/// magic-link URLs.
#[derive(Clone, Debug)]
pub struct EmailConfig {
    selection: EmailTransportSelection,
    from: String,
    site_url: String,
}

impl Default for EmailConfig {
    /// The safe, back-compat default: [`LogTransport::from_env`] — no
    /// config needed, no network ever touched, mirrors `CaptchaConfig::
    /// default`/`OAuthConfig::default` being the inert/off-by-default case.
    fn default() -> Self {
        Self {
            selection: EmailTransportSelection::Log(LogTransport::from_env()),
            from: "unidb@localhost".to_string(),
            site_url: "http://localhost:8080".to_string(),
        }
    }
}

impl EmailConfig {
    /// Explicit dev-inbox construction — used by tests so each test gets an
    /// isolated inbox file (and a deterministic `from`/`site_url`) without
    /// touching process-global env vars.
    pub fn with_log_transport(
        path: impl Into<PathBuf>,
        from: impl Into<String>,
        site_url: impl Into<String>,
    ) -> Self {
        Self {
            selection: EmailTransportSelection::Log(LogTransport::new(path)),
            from: from.into(),
            site_url: site_url.into(),
        }
    }

    /// `UNIDB_EMAIL_TRANSPORT` (default `log`) / `UNIDB_EMAIL_SITE_URL` /
    /// the `UNIDB_SMTP_*` family — see the module doc for each. `Err` with a
    /// human-readable message for `UNIDB_EMAIL_TRANSPORT=smtp` missing
    /// `UNIDB_SMTP_HOST`/`UNIDB_SMTP_FROM`, or an unrecognized transport
    /// name — the caller (`unidb-server.rs`) refuses to start in that case,
    /// same posture as a malformed `UNIDB_MASTER_KEY`.
    pub fn from_env() -> std::result::Result<Self, String> {
        let transport = std::env::var("UNIDB_EMAIL_TRANSPORT")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "log".to_string());
        let site_url = std::env::var("UNIDB_EMAIL_SITE_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "http://localhost:8080".to_string());
        let from = std::env::var("UNIDB_SMTP_FROM")
            .ok()
            .filter(|s| !s.is_empty());

        match transport.to_ascii_lowercase().as_str() {
            "log" => Ok(Self {
                selection: EmailTransportSelection::Log(LogTransport::from_env()),
                from: from.unwrap_or_else(|| "unidb@localhost".to_string()),
                site_url,
            }),
            "smtp" => {
                let host = std::env::var("UNIDB_SMTP_HOST")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        "UNIDB_EMAIL_TRANSPORT=smtp requires UNIDB_SMTP_HOST".to_string()
                    })?;
                let from = from.ok_or_else(|| {
                    "UNIDB_EMAIL_TRANSPORT=smtp requires UNIDB_SMTP_FROM".to_string()
                })?;
                let port: u16 = std::env::var("UNIDB_SMTP_PORT")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(587);
                let username = std::env::var("UNIDB_SMTP_USERNAME").unwrap_or_default();
                let env_password = std::env::var("UNIDB_SMTP_PASSWORD").unwrap_or_default();
                let tls_mode = SmtpTlsMode::from_env();
                Ok(Self {
                    selection: EmailTransportSelection::Smtp(SmtpConfig::new(
                        host,
                        port,
                        username,
                        env_password,
                        tls_mode,
                    )),
                    from,
                    site_url,
                })
            }
            other => Err(format!(
                "unrecognized UNIDB_EMAIL_TRANSPORT '{other}' (expected 'log' or 'smtp')"
            )),
        }
    }

    /// Dispatch `msg` to the configured transport, resolving the SMTP
    /// password vault-first when the SMTP transport is selected (see
    /// [`resolve_smtp_password`]). Log-transport sends never touch `engine`.
    async fn dispatch(&self, engine: &EngineHandle, msg: EmailMessage) -> Result<()> {
        match &self.selection {
            EmailTransportSelection::Log(log) => log.send(&msg).await,
            EmailTransportSelection::Smtp(cfg) => {
                let password = resolve_smtp_password(engine, cfg).await?;
                SmtpTransport::new(cfg, password).send(&msg).await
            }
        }
    }

    /// Render the `"recovery"` template and send it to `to_email` (item 138
    /// — `POST /auth/recover`). `to_username` feeds `{{user}}`; the link is
    /// `{site_url}/auth/verify?token=<raw_token>`.
    pub async fn send_recovery_email(
        &self,
        engine: &EngineHandle,
        to_username: &str,
        to_email: &str,
        raw_token: &str,
    ) -> Result<()> {
        let link = format!("{}/auth/verify?token={}", self.site_url, raw_token);
        let template = load_template("recovery");
        let vars = TemplateVars {
            link: Some(&link),
            code: None,
            user: Some(to_username),
            site_url: Some(&self.site_url),
        };
        let (subject, text_body, html_body) = render_template(&template, &vars);
        let msg = EmailMessage {
            to: to_email.to_string(),
            from: self.from.clone(),
            subject,
            text_body,
            html_body,
        };
        self.dispatch(engine, msg).await
    }

    /// Render the `"magiclink"` template and send it to `to_email` (item
    /// 138 — `POST /auth/magiclink`). The link is
    /// `{site_url}/auth/magiclink/verify?token=<raw_token>`.
    pub async fn send_magiclink_email(
        &self,
        engine: &EngineHandle,
        to_username: &str,
        to_email: &str,
        raw_token: &str,
    ) -> Result<()> {
        let link = format!(
            "{}/auth/magiclink/verify?token={}",
            self.site_url, raw_token
        );
        let template = load_template("magiclink");
        let vars = TemplateVars {
            link: Some(&link),
            code: None,
            user: Some(to_username),
            site_url: Some(&self.site_url),
        };
        let (subject, text_body, html_body) = render_template(&template, &vars);
        let msg = EmailMessage {
            to: to_email.to_string(),
            from: self.from.clone(),
            subject,
            text_body,
            html_body,
        };
        self.dispatch(engine, msg).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitute_replaces_every_known_variable() {
        let vars = TemplateVars {
            link: Some("https://example.test/verify?token=abc"),
            code: Some("123456"),
            user: Some("alice"),
            site_url: Some("https://example.test"),
        };
        let out = substitute(
            "Hi {{user}}, visit {{link}} or use code {{code}} on {{site_url}}",
            &vars,
            false,
        );
        assert_eq!(
            out,
            "Hi alice, visit https://example.test/verify?token=abc or use code 123456 on https://example.test"
        );
    }

    #[test]
    fn substitute_missing_variable_becomes_empty_not_a_panic_or_literal() {
        let vars = TemplateVars::default();
        let out = substitute("Hi {{user}}!", &vars, false);
        assert_eq!(out, "Hi !");
    }

    #[test]
    fn substitute_html_escapes_values_but_not_template_markup() {
        let vars = TemplateVars {
            link: Some("https://example.test/x?a=1&b=2"),
            user: Some("<script>alert(1)</script>"),
            code: None,
            site_url: None,
        };
        let out = substitute("<p>Hi {{user}}</p><a href=\"{{link}}\">go</a>", &vars, true);
        // The template's own markup is untouched...
        assert!(out.starts_with("<p>Hi "));
        assert!(out.contains("<a href=\""));
        // ...but the substituted values are escaped, so no injected tag or
        // unescaped '&' survives.
        assert!(!out.contains("<script>"));
        assert!(out.contains("&lt;script&gt;"));
        assert!(out.contains("&amp;b=2"));
    }

    #[test]
    fn default_recovery_template_renders_link_and_user() {
        let template = default_template("recovery");
        let vars = TemplateVars {
            link: Some("https://example.test/auth/verify?token=tok123"),
            user: Some("bob"),
            code: None,
            site_url: Some("https://example.test"),
        };
        let (subject, text, html) = render_template(&template, &vars);
        assert!(subject.to_lowercase().contains("password"));
        assert!(text.contains("bob"));
        assert!(text.contains("https://example.test/auth/verify?token=tok123"));
        assert!(html.unwrap().contains("tok123"));
    }

    #[test]
    fn unrecognized_template_name_falls_back_to_a_generic_link_only_template() {
        let template = default_template("something-not-built-in");
        let vars = TemplateVars {
            link: Some("https://example.test/z"),
            ..Default::default()
        };
        let (_subject, text, html) = render_template(&template, &vars);
        assert_eq!(text, "https://example.test/z");
        assert!(html.is_none());
    }

    #[tokio::test]
    async fn log_transport_writes_one_json_line_per_send() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.jsonl");
        let transport = LogTransport::new(&path);
        let msg = EmailMessage {
            to: "user@example.test".to_string(),
            from: "unidb@localhost".to_string(),
            subject: "Subject One".to_string(),
            text_body: "Body one with a token=abc123".to_string(),
            html_body: None,
        };
        transport.send(&msg).await.unwrap();
        transport
            .send(&EmailMessage {
                subject: "Subject Two".to_string(),
                ..msg.clone()
            })
            .await
            .unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["to"], "user@example.test");
        assert_eq!(first["subject"], "Subject One");
        assert!(first["text_body"]
            .as_str()
            .unwrap()
            .contains("token=abc123"));
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["subject"], "Subject Two");
    }

    #[test]
    fn email_message_debug_redacts_subject_and_bodies() {
        let msg = EmailMessage {
            to: "user@example.test".to_string(),
            from: "unidb@localhost".to_string(),
            subject: "top secret subject".to_string(),
            text_body: "contains token=super-secret-token".to_string(),
            html_body: Some("<p>token=super-secret-token</p>".to_string()),
        };
        let debug_str = format!("{msg:?}");
        assert!(!debug_str.contains("top secret subject"));
        assert!(!debug_str.contains("super-secret-token"));
        assert!(debug_str.contains("redacted"));
        // Non-secret envelope fields still show through.
        assert!(debug_str.contains("user@example.test"));
    }

    #[test]
    fn smtp_config_debug_never_leaks_the_password() {
        let cfg = SmtpConfig::new(
            "smtp.example.test",
            587,
            "smtp-user",
            "super-secret-password",
            SmtpTlsMode::Starttls,
        );
        let debug_str = format!("{cfg:?}");
        assert!(!debug_str.contains("super-secret-password"));
        assert!(debug_str.contains("redacted"));
    }

    #[test]
    fn from_env_smtp_without_host_errors_clearly() {
        static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("UNIDB_EMAIL_TRANSPORT", "smtp");
        std::env::remove_var("UNIDB_SMTP_HOST");
        let result = EmailConfig::from_env();
        std::env::remove_var("UNIDB_EMAIL_TRANSPORT");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("UNIDB_SMTP_HOST"));
    }

    #[test]
    fn from_env_defaults_to_log_transport() {
        static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("UNIDB_EMAIL_TRANSPORT");
        let cfg = EmailConfig::from_env().unwrap();
        assert!(matches!(cfg.selection, EmailTransportSelection::Log(_)));
    }
}
