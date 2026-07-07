# Remote Artifact Cache Protocol v1

> **Experimental** — read-through only. Upload is reserved for a follow-up plan.

## Base URL and path

```
GET <base>/v1/artifacts/sha512/<128-lowercase-hex>
Authorization: Bearer <token>   # only when configured
Accept: application/octet-stream
```

- `<base>` is an absolute HTTPS URL.
- The path is always `/v1/artifacts/sha512/` followed by the 128-character
  lowercase hex SHA-512 digest of the raw artifact (`.tgz` archive).
- No package name, registry URL, project path, graph ID, or other metadata
  is encoded in the request path.
- A trailing slash on the base URL is normalized.
- Userinfo (`username:password@`), query strings, and fragments are rejected
  at configuration time.

## Responses

| Status | Meaning | Behaviour |
|--------|---------|----------|
| 200 | full `.tgz` bytes | Stream to temp file; verify SHA-512 matches request path; publish atomically on match. |
| 404 | cache miss | Clean temp file; fall back to origin registry tarball download. |
| 401 / 403 | auth error | Warn once per command (redacted), record `remote_cache_error` metric, fall back to origin. |
| other 4xx / 5xx / transport error | upstream failure | Warn in redacted form, record metric, fall back to origin. |

## Redirects

Redirects are rejected at the HTTP client layer. A cache object URL must not
redirect. If a redirect response is received, the client does not follow it
and falls back to origin.

## Authentication

- Token is supplied via `BPM_REMOTE_CACHE_TOKEN` environment variable only.
- The token is never accepted on the command line, never written to project
  files, and never serialized in debug output or error messages.
- The `Authorization: Bearer <token>` header is attached only to cache
  endpoint requests. It is never forwarded to registry origins and never
  follows redirects.

## Offline mode

When `--offline` or equivalent cache-mode forbids network, the remote cache
client is not constructed and no remote requests are made. Existing offline
behaviour is preserved exactly.

## Integrity and security

- The requested SHA-512 digest is the sole object selector. The cache is a
  key-value store keyed by digest, not by package identity.
- Every response body is streamed to a unique store-temp file and rehashed
  before local atomic publication via `publish_file`. A digest mismatch
  deletes the temp file and returns a corruption error.
- The store's per-digest advisory lock serialises concurrent writes for the
  same digest, so two concurrent cache hits (or a concurrent cache hit and
  origin download) produce one correct published artifact.

## Concurrency

Multiple processes or threads requesting the same digest concurrently share
the per-digest lock. The first to acquire the lock performs the remote fetch
(or local hit); subsequent acquisitions short-circuit to the already-published
artifact.

## Cache miss / error fallback

A miss, auth error, transport failure, or corruption always falls back to the
configured origin tarball download path. The fallback proceeds through the
existing `ensure_artifact_with_client` path with the same store-level locking,
so fallback integrity is identical to a normal origin install.

## Object scope

This protocol carries **raw artifacts only** (`.tgz` archives). It does not
transfer:

- Package images (extracted directories with layout metadata)
- Derived lifecycle images
- Graph volumes (linked `node_modules` directories)
- Plan files
- Metadata / packuments
- Manifests or lockfiles

Those object kinds have different portability, versioning, and security
invariants and are deferred.

## Reserved for future versions

A conditional PUT with `If-None-Match: *` may be specified in a follow-up
plan for idempotent upload. This protocol version does not implement upload.
The upload path would be:

```
PUT <base>/v1/artifacts/sha512/<128-lowercase-hex>
Content-Type: application/octet-stream
If-None-Match: *
```

A `409 Conflict` response would indicate the digest already exists.

## Compatibility and versioning

- The `v1` path segment is a protocol version. Future incompatible changes
  will increment it.
- The 128-hex digest format is fixed for SHA-512. Future hash algorithms will
  use a distinct path prefix such as `/v2/artifacts/sha3-512/...`.

## Threat model

| Threat | Mitigation |
|--------|------------|
| Malicious cache bytes | SHA-512 rehash before publish; mismatch deletes temp and falls back to origin. |
| Digest path injection | Hex validation at path construction; no interpolation into filesystem paths. |
| Token leakage via logs | Redacted `Debug` impl on config/errors; no token on CLI or in project files. |
| Token leakage via redirect | Redirects are refused; auth header never reaches another origin. |
| Cache outage | Explicit fallback to origin; recorded metric for observability. |
| Partial response | Client detects truncated stream; temp file is deleted and origin fallback used. |
| Oversized response | Bounded by available disk; store temp directory enforces per-digest temp creation. |
| Concurrent local writers | Per-digest advisory lock serialises the ensure/publish step. |
