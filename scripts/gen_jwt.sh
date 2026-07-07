#!/usr/bin/env bash
# Generate a verify-only HS256 JWT for unidb-server's auth (src/server/auth.rs).
#
# Deliberately pure bash + openssl — no Python/PyJWT dependency, since that
# snippet (previously in README.md) breaks on any machine without PyJWT
# installed ("ModuleNotFoundError: No module named 'jwt'"). openssl is
# already required by scripts/bench_server.sh and is present on effectively
# every dev machine and CI image by default.
#
# Usage:
#   UNIDB_JWT_SECRET=dev-secret ./scripts/gen_jwt.sh [subject] [ttl_seconds]
#
# Prints the token to stdout, nothing else — safe to use directly in
# $(...) command substitution, e.g.:
#   TOKEN=$(UNIDB_JWT_SECRET=dev-secret ./scripts/gen_jwt.sh dev 3600)
#   curl -H "Authorization: Bearer $TOKEN" ...

set -euo pipefail

SECRET="${UNIDB_JWT_SECRET:?set UNIDB_JWT_SECRET to match the server configuration}"
SUBJECT="${1:-dev}"
TTL="${2:-3600}"

b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }

header=$(printf '{"alg":"HS256","typ":"JWT"}' | b64url)
payload=$(printf '{"sub":"%s","exp":%d}' "${SUBJECT}" "$(($(date +%s) + TTL))" | b64url)
signing_input="${header}.${payload}"
signature=$(printf '%s' "${signing_input}" | openssl dgst -sha256 -hmac "${SECRET}" -binary | b64url)

printf '%s.%s\n' "${signing_input}" "${signature}"
