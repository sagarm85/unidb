# Email transport + templates (auth email flows foundation)

**Type:** Improvement
**Status:** IN PROGRESS

> Wave-1 lead item of the free roadmap (`137_supabase_parity_free_roadmap.md`).
> unidb currently has **no way to send email at all**, which blocks the whole
> Supabase email-auth cluster (magic link, email OTP, password reset, email
> confirmation, email change). This adds the foundational **pluggable email
> transport + template system**, then wires the first flow(s) on top.
>
> **Free by design:** the transport is provider-agnostic — SMTP (self-hosted or
> any provider, incl. free tiers) or a dev/log transport for local/testing. The
> engine never requires a paid service. Sending email is an outbound network /
> control-plane action (same posture as OAuth's HTTP calls) — no WAL/MVCC/heap
> impact; crash harness stays 54/54.

## Scope (this PR)

1. **`EmailTransport` trait** — `async fn send(&self, msg: &EmailMessage) -> Result<()>`
   where `EmailMessage { to, from, subject, text_body, html_body? }`.
   - **`SmtpTransport`** — real delivery. Config from env: `UNIDB_SMTP_HOST`,
     `UNIDB_SMTP_PORT`, `UNIDB_SMTP_USERNAME`, `UNIDB_SMTP_PASSWORD` (password
     **vault-first** via `smtp.password`, then env — same order as OAuth/CAPTCHA
     secrets, item 129), `UNIDB_SMTP_FROM`, TLS/STARTTLS toggle. Use a
     well-maintained crate (`lettre`) behind the `server` feature.
   - **`LogTransport`** — writes the rendered email to `tracing`/a dev inbox file
     instead of sending; the default when no SMTP is configured, so local/dev/
     tests need no mail server (mirrors Supabase's Inbucket/Mailpit).
   - Selection: `UNIDB_EMAIL_TRANSPORT=smtp|log` (default `log`; `smtp` requires
     the host to be set or it errors clearly at startup, like OAuth providers).
2. **Template system** — named templates (subject + text/HTML body) with safe
   substitution of a small variable set (`{{link}}`, `{{code}}`, `{{user}}`,
   `{{site_url}}`). Built-in defaults per flow; overridable via a templates dir
   (`UNIDB_EMAIL_TEMPLATES_DIR`) — do NOT interpolate untrusted values into HTML
   without escaping.
3. **First flow — password reset (self-service, email-based):**
   - `POST /auth/recover` — body `{ email }`. Always returns `200` regardless of
     whether the address exists (no account-enumeration oracle). If it exists,
     mint a single-use, hash-only-persisted, short-TTL recovery token (reuse the
     refresh/oauth-state token machinery — 256-bit OS-CSPRNG, SHA-256 stored),
     render the `recovery` template with a link carrying the token, and send it.
   - `POST /auth/verify` (or `/auth/recover/confirm`) — body `{ token,
     new_password }`. Validates + consumes the token, sets the new argon2id
     credential via the existing `ALTER USER … PASSWORD` / `set_password` path,
     revokes existing sessions (recommended). Rate-limited (item I1) + CAPTCHA-
     eligible (item I2).
4. **Magic link (include if clean, else a fast follow-up):**
   - `POST /auth/magiclink` — `{ email }`, always `200`; if the user exists, mint
     a single-use short-TTL login token, email a link. A `GET/POST` verify route
     redeems it for a real session via the existing `issue_token_pair` path.

## Non-goals (this PR — follow-ups, tracked in 137)
- Email OTP (numeric code) · email confirmation on signup · email-change flow —
  same machinery, land next once the transport + templates + one flow are proven.
- HTML email theming beyond a minimal default.

## Correctness / security
- **No account enumeration:** recover/magiclink always `200`; the email is the
  only signal.
- Tokens: single-use, short TTL (e.g. 1h recovery / 15m magic link), **hash-only
  persisted**, invalidated on use — reuse item-121 A4 / item-128 OAuth-state
  patterns; never log a token or a rendered link at info level.
- SMTP password redacted from `Debug`/audit; vault-first resolution.
- All new DTOs `Debug`-redact secrets.

## Acceptance
- With `UNIDB_EMAIL_TRANSPORT=log`, `POST /auth/recover` for an existing user
  writes a rendered email (captured via the log/dev-inbox) containing a valid
  token; `POST /auth/verify` with that token sets the new password and the user
  can log in with it; an unknown email still returns `200` and sends nothing.
- Enumeration test: known vs unknown email are response-indistinguishable.
- Token is single-use (second verify fails) and TTL-expiring.
- `tests/item138_email_auth.rs` (`#![cfg(feature = "server")]` first line) using
  the **log/dev transport** (NO real SMTP, NO network) — capture the outbound
  email from the transport.
- Every pre-existing `item100/item121/item127/item128` auth test unchanged.
- **Crash 54/54**; `cargo test --no-run` (no features) + `clippy --all-features
  --all-targets -D warnings` + `fmt` clean.
- `docs/REST_API.md` (new auth email routes + transport env vars), `README.md`
  auth bullet, `docs/backlog/137` Wave-1 line, this Status flipped on merge.
