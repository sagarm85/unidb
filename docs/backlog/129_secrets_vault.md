# Encrypt-at-rest secrets vault

**Type:** Improvement
**Status:** SHIPPED (2026-08-01) — see `PROGRESS.md` note below (no dedicated
entry per this task's scope — see the commit for the metrics-free rationale
matching items 124–128's "no `PROGRESS.md` entry" precedent).

> Workstream I3 (item 120, `120_supabase_parity_roadmap.md`'s Workstream I —
> Hardening & ops). Config secrets (today: OAuth client secrets — the seam
> `src/server/oauth.rs` left at item 128; later: SMTP credentials) should be
> encrypted at rest rather than sitting in plaintext env vars / `roles.json`.

## What shipped

1. **Vault crypto (`src/vault.rs`).** AES-256-GCM (RustCrypto `aes-gcm`
   0.11). Master key from `UNIDB_MASTER_KEY` — base64 (padded or unpadded)
   or hex, must decode to exactly 32 bytes, validated with a clear error
   naming the problem (never the input itself, to avoid echoing a near-miss
   typo of the real key). Each encryption draws a fresh random 96-bit nonce
   from the OS CSPRNG (the same `OsRng` `src/authz/mod.rs` already uses);
   the stored blob is `base64(nonce || ciphertext || tag)`. Decrypt verifies
   the GCM authentication tag — any tamper is detected and rejected, never
   silently decrypted to garbage. No `UNIDB_MASTER_KEY` → vault **disabled**
   (startup `warn!`, never a hard crash); a secret genuinely stored while
   enabled can only ever be read back with the same key — wrong/rotated key
   or a tampered blob both fail closed (`Err`, never a plaintext fallback).
   Base64 is hand-rolled (RFC 4648) rather than a new unconditional
   dependency, same "hand-roll the small thing" call `src/authz/mod.rs`
   already makes for TOTP's base32.
2. **`SecretStore`.** A `name -> encrypted_blob` map added to `AuthState`
   (`secrets` field, `#[serde(default)]` — an existing `roles.json` loads
   unchanged, no `FORMAT_VERSION` bump), persisted in the same
   `roles.json` control-plane store as credentials/sessions/MFA/OAuth
   identities. `RoleStore::set_secret`/`get_secret`/`has_secret`/
   `secret_names`; `Engine` delegates the same four. The manual `AuthState`
   `Debug` impl redacts `secrets` to a count, matching
   `credentials`/`sessions`/`mfa`.
3. **`unidb-vault` CLI (`src/bin/unidb-vault.rs`).** `set <name>` (plaintext
   from stdin if piped, else `UNIDB_VAULT_SECRET_VALUE` — never a CLI
   argument, so it never lands in shell history/`ps`; prints only `<name>:
   stored`), `has <name>` (stored/not stored, never the value), `list`
   (names only). Not `server`-feature-gated — same posture as
   `unidb-migrate`: opens the sync embedded `Engine` directly.
4. **OAuth seam wired (`src/server/oauth.rs`, `src/server/handlers.rs`).**
   `resolve_client_secret(engine, provider, cfg)` checks the vault first
   (secret name `oauth.<provider>.client_secret`), falling back to the
   `UNIDB_OAUTH_<PROVIDER>_CLIENT_SECRET` env value when nothing is stored.
   `_CLIENT_SECRET` is now *optional* in `provider_from_env` (client_id +
   redirect_uri are enough to configure a provider), so an operator can go
   vault-only. A vault secret that was stored but can't be decrypted is a
   hard `502 OAUTH_PROVIDER_UNAVAILABLE` — never a silent plaintext
   fallback. With no `UNIDB_MASTER_KEY` and nothing ever stored, resolution
   always takes the env branch — D1's original behavior is unchanged
   byte-for-byte (proven by `item129_vault.rs`'s disabled-vault test).

## Verification

- `src/vault.rs` unit tests (11): b64/hex codec round-trips + garbage
  rejection, master-key parsing (both encodings, wrong length, garbage),
  disabled-vault error-not-crash, encrypt→decrypt round-trip, fresh nonce
  per call, wrong-key/tampered-ciphertext fail-closed, `Debug` never leaks
  key material.
- `src/authz/mod.rs` unit tests (7 new): set/get round-trip, `None` for
  never-stored, `has_secret`/`secret_names` never expose values, set fails
  closed when vault disabled, get fails closed on a key change, ciphertext
  never appears in `Debug` or the persisted `roles.json`, back-compat load
  of a pre-129 `roles.json` with no `secrets` key.
- `tests/item129_vault.rs` (3, `#![cfg(feature = "server")]`, mock OAuth
  provider, no real network/secrets): token exchange uses the vault secret
  when stored; falls back to the env secret when not in the vault (vault
  enabled but empty for that provider); with no master key at all, the
  vault is disabled (`set_secret` errors closed) and OAuth behavior is
  byte-for-byte unchanged from before item 129.
- Gates: `cargo test --no-run` (no features) clean; `cargo build
  --all-features` clean; `cargo clippy --all-features --all-targets -- -D
  warnings` clean; `cargo fmt --all` clean; crash harness 54/54.

## Follow-ups (not in this item's scope)

- SMTP credentials (D5, email flows) reuse this same `SecretStore` once
  that workstream starts — no vault changes needed, just a new secret name
  convention (`smtp.<field>`, mirroring `oauth.<provider>.client_secret`).
- Master-key rotation (re-encrypt every stored secret under a new key) is
  not built — today, rotating `UNIDB_MASTER_KEY` requires re-`set`-ting
  every secret from its plaintext source. A `unidb-vault rotate` subcommand
  is a reasonable future addition if the vault sees multi-secret production
  use.
- No HTTP route exposes `set`/`get`/`list` — vault management is CLI-only
  by design (this task's explicit ask), matching `unidb-migrate`'s posture.
  A future `unidb-studio` "Secrets" panel would need a thin admin-only HTTP
  wrapper around the same `Engine` methods.
