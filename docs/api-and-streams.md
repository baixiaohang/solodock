# SoloDock API and real-time streams

> English (authoritative) · [简体中文](zh-CN/api-and-streams.md)

SoloDock's management API and embedded UI share the canonical HTTPS authority from `public_origin`. In production, the process listens only on loopback and relies on an external tunnel/WAF/TLS endpoint for public access. The reverse proxy must preserve that external `Host`; `Forwarded`, `X-Forwarded-Host`, and similar headers never authorize routing. The application still enforces authority, authentication, Origin, CSRF, request-size, and authorization checks.

## Authentication boundary

- First startup creates a one-time `bootstrap.token`; administrator credentials can be initialized only through the bootstrap endpoint.
- Passwords use Argon2id, and there is only one administrator account.
- Login establishes a Secure, HttpOnly, SameSite=Strict session cookie.
- An authenticated mutation also requires an `Origin` exactly matching `public_origin` and a double-submit `X-CSRF-Token`.
- Sessions expire and can be logged out or revoked globally; SSE heartbeat revalidates them.
- An authenticated administrator can rotate the password only by proving the current password. A successful rotation revokes every session, including the caller, and expires both browser cookies.
- Login throttling, successes/failures, and sensitive administrative actions are audited without recording passwords, cookies, or secrets.

Authentication protocol endpoints do not use the business `Idempotency-Key`. Singleton credentials, one-time tokens, or random sessions provide their replay boundaries.

## Management API contract

Requests with the management authority may reach management API, authentication, SSE, UI/assets, `/healthz`, and `/favicon.svg`, but never `/hooks/**`. The optional webhook authority accepts only exact `POST /hooks/v1/apps/<canonical-lowercase-UUID>/registry`. When the loopback listen authority differs from management, it permanently accepts only exact `GET /healthz` and `GET /favicon.svg` for local updater compatibility. Every other authority/path/method combination returns a body-minimal `404` with `Cache-Control: no-store` without revealing configured authorities.

Every `/api/v1/**` response uses `Cache-Control: no-store` and an allowlisted DTO rather than serializing Docker, Registry, or internal storage models directly. Errors keep a stable code, sanitized message, and `request_id`. Configuration validation may also include backward-compatible `issues`, each containing only a field path, stable code, and safe explanation without echoing input values, secrets, credentials, or host paths:

```json
{
  "code": "APP_BUSY",
  "message": "The application already has an active mutation",
  "request_id": "...",
  "issues": [
    {
      "path": "health.http.retries",
      "code": "HEALTH_RETRIES_OUT_OF_RANGE",
      "message": "The retry count is outside the allowed range"
    }
  ]
}
```

The Web client handles an HTTP `401` before attempting to parse its body, so an expired or revoked session returns to authentication even when a tunnel or WAF substitutes HTML, an empty body, or malformed JSON. Other non-success responses are parsed as API errors only for `application/json` or `+json` media types and only when the stable error envelope has the expected runtime shape. Otherwise the client preserves the real HTTP status but uses a local `HTTP_ERROR` message without displaying the response body. A bounded, safe `X-Request-ID` header takes precedence over a valid JSON `request_id`; unsafe identifiers are discarded.

Logout and global session revocation change the browser's authenticated state only after the server confirms success. A `401` still follows the common unauthorized path. Network, CSRF/WAF, throttling, and server failures keep the current authenticated view and present a retryable sanitized error; transport failures explicitly remain an unknown result rather than being reported as success.

`PUT /api/v1/me/password` requires an authenticated session, exact Origin, double-submit CSRF, and a JSON object containing only `current_password` and `new_password`. The new password uses the same 14–128 Unicode scalar and 512-byte policy as bootstrap. A wrong current password returns HTTP 403 `CURRENT_PASSWORD_INVALID`; the shared authentication cooldown returns HTTP 429 `AUTH_COOLDOWN`. The endpoint does not use or require an `Idempotency-Key`. On success it atomically updates the Argon2id hash, deletes all sessions, clears the shared throttle, appends a sanitized `auth.password_change` audit event, expires both managed cookies, and returns 204. Any transaction failure preserves the old hash, sessions, throttle, and audit state and does not expire cookies.

The Web Settings security form holds password values only in component memory and sends confirmation only as a client-side check. A confirmed 204 returns to login. Deterministic JSON errors remain in the authenticated shell; an unconfirmed network or proxy result is not retried automatically and directs the administrator to reload and test the new password before trying again.

Persistent business mutations require a safe 16–128-byte ASCII `Idempotency-Key`. SQLite stores the request fingerprint/HMAC, operation state, and sanitized response. The same key and identical request may replay; changing the body, route, or method conflicts. Frontend retry identities keep only hashes of Registry credentials and webhook secrets, while the backend API uses zeroizing wrappers for managed parsed buffers.

The Web client retains an idempotency key for manual retry only when the mutation outcome is unknown: a network rejection or abort, an edge/proxy response that is not a validated JSON error envelope, an unexpected success status, or any HTTP 5xx response. A runtime-validated backend JSON 4xx response proves the mutation was rejected before application, so the next manual attempt receives a new key. Confirmed success also clears the key. This classification is made only at the shared API boundary; raw response bodies and write-only secrets never participate in user-visible error handling.

Terminal replay records normally expire after 24 hours, but cleanup is a low-frequency service operation rather than a side effect of claiming an unrelated mutation. Before each bounded cleanup batch, SoloDock inventories every finalizer-owned filesystem artifact and retains the exact operation proof it references. If any application, credential, or webhook artifact inventory is incomplete or invalid, that cleanup cycle deletes nothing. Pending and interrupted records are never time-collected.

Endpoints are grouped around stable resources:

- application catalog, detail, draft, validation, lifecycle, and deletion;
- built-in application presets and read-only OCI image-config suggestions;
- Registry credentials;
- deployment scheduling, history, detail, and rollback;
- per-application webhook status, configure, and revoke;
- global display settings at `GET/PUT /api/v1/settings`;
- system health, drift, and installation identity;
- events, logs, and stats SSE.

For exact routes, body limits, and fields, use `src/api/mod.rs`, DTOs, and generated frontend types. This document intentionally does not duplicate a route table that would drift.

`POST /api/v1/apps` accepts only an immutable 1–20-character `slug` and returns an `UNCONFIGURED` application. The first draft mutation accepts `expected_revision: null`; subsequent mutations require the exact UUID. Both paths share revision and idempotency guards. Draft input includes `stop_grace_period_seconds` in `1..=600` (default `10`), `owned_default_network` and `service_discovery_enabled` enabled by default for new revisions, and structured external attachments. Compose preflight returns the final grace period, network mode, attachments, platform DNS alias, warnings, and versioned resource identity.

`GET /api/v1/app-presets` returns only versioned public descriptors. `POST /api/v1/apps/from-preset` uses write-only variables to create a normal revision. PostgreSQL v1 supports majors 18 and 17, mounting `/var/lib/postgresql` and `/var/lib/postgresql/data` respectively, never uses `latest`, and never echoes the password. The Web UI then calls the existing deployment mutation with a separate stable idempotency key. If creation succeeds and deployment fails, the recoverable application remains.

`POST /api/v1/images/inspect-config` reuses Registry credentials and the manifest resolver, validates config-blob digest/size/media type, and projects only exposed ports, volume targets, healthcheck presence, user, and stop signal. The Web UI calls this read-only POST through the common JSON/CSRF mutation helper and submits the selected credential reference unchanged. It needs no durable idempotency ledger. The API does not return image Env, labels, entrypoint/command, or write a revision. Explicitly accepting suggestions still uses the normal draft mutation.

Application details return resource names generated by the versioned naming helper from the immutable slug and UUID. They separately show the immutable expected network plan selected from actual release identity, expected owned identity, Docker's actual driver/bridge, and container attachments. A different attachment-name set reports `NETWORK_ATTACHMENT_MISMATCH`; a mismatched driver or explicit bridge option reports `NETWORK_BRIDGE_IDENTITY_MISMATCH`; and a missing expected alias on an external attachment reports `NETWORK_ALIAS_MISMATCH`. Incomplete inspection leaves observation incomplete instead of fabricating a mismatch.

`GET /healthz` returns only minimal process liveness. Authenticated system health exposes Docker capability, filesystem recovery, projection, deployment, polling, webhook, host `MemAvailable`, disk, credential, and stream state. `GET /api/v1/system/installation` is also authenticated and returns only a validated `stable`, `main`, `development`, or `unknown` channel plus canonical version/source/package identity fields. On every request it reads the fixed `/usr/local/bin/solodock` managed symlink and the selected identity-qualified generation's `INSTALL_MANIFEST`; the generation name must bind the manifest's version and package identity. It never accepts a path from the request or reflects unvalidated file content. A normal source run without a managed installation is `development`, while a missing, unsafe, damaged, or inconsistent managed manifest is `unknown`. Neither condition prevents the rest of the console from operating. The authenticated control plane can start without Docker; the catalog retains filesystem facts, and drift that cannot be observed completely is explicitly incomplete.

Packaged helpers obtain the probe URL through the side-effect-free `solodock inspect-packaged-config /etc/solodock/config.toml` command, not an HTTP endpoint or a shell TOML parser. Its fixed, versioned line record contains only the normalized local health URL/authority and management authority. The updater probes exact `/healthz` and `/favicon.svg` on that configured IPv4 or bracketed IPv6 loopback authority; there is no separate `--health-url` override.

`GET /api/v1/settings` returns revision, display timezone, the IANA list, dynamic `allowed_bind_roots`, `slug_max_length`, supported mount types, and Rust-domain `configuration_limits.health`. The Web UI uses only these capabilities for health-field min/max/default values and prevents configuration save if they are missing. `PUT` atomically updates timezone and bind roots. A root referenced by a revision cannot be removed, and scan or artifact-read failure fails closed. The settings mutation requires `expected_revision`, `Idempotency-Key`, exact Origin, session, and CSRF. Display settings affect only Web formatting; API and SSE timestamps remain RFC3339 UTC.

## Two-phase deletion

Deletion cannot trust only the UI's current view. Under coordination lock, preview builds canonical facts from fresh filesystem data, verified active/pending configs, webhook artifacts, and Docker observation and issues a short-lived confirmation token.

DELETE submits the token, slug, and container-removal choice. The service recomputes the complete facts hash before token consumption and before the filesystem tombstone. Network facts include mode-derived owned/external kind, aliases, active/pending/draft scope, and existence, so changes to attachment mode or aliases invalidate the token. Changed facts return stale/conflict without deletion. The default only unregisters; explicit removal acts only on the exact owned container bound to the token and preserves all volumes, bind contents, and networks.

A successful tombstone must first publish catalog removal, then finalize exactly. If projection or fsync is uncertain, the tombstone remains for reconciler/startup convergence. Application deletion permanently removes its managed config/secrets and webhook secret, so the locked preview must state that clearly.

## Common SSE boundary

Events, logs, and stats are server-to-client SSE only. There is no WebSocket, terminal, shell, or exec capability.

Current `StreamGate` limits are:

| Scope | Limit |
| --- | ---: |
| Global connections | 24 |
| Per session | 8 |
| Events global / per application | 16 / 4 |
| Logs global / per application | 8 / 2 |
| Stats global / per application | 8 / 2 |

Before establishing a stream, SoloDock revalidates the session, application catalog, and ownership of the exact full container ID. Docker unavailability or ownership errors discovered before response headers produce a stable error. A 15-second heartbeat revalidates the session; expiration or revoke-all closes the connection.

Application deletion blocks new streams and waits for subscribers and logs/stats producers to exit. Failure before the tombstone rolls back the stream generation so the still-registered application can be subscribed again. Commit permanently closes it.

## Events

Docker events project only allowlisted fields for matching SoloDock ownership. A per-process boot UUID and monotonic sequence form the cursor. A bounded ring can replay within the same process; otherwise it sends reset rather than fabricating continuity. A slow consumer that exceeds its bounded queue receives `SLOW_CONSUMER` and is disconnected.

## Logs

Logs support bounded tail/since cursors and accept no arbitrary Docker arguments. The framer reconstructs logical lines, then performs a one-pass bounded byte-level redaction against all secrets known to SoloDock, including patterns split across Docker chunks.

- A raw line over 64 KiB is omitted entirely.
- A normal message is limited to 16 KiB.
- NUL and terminal-control sequences are removed.
- Output includes only allowlisted fields such as stream, timestamp, and message.

Application-generated secrets the control plane has never held cannot be identified reliably and are outside the redaction guarantee.

## Stats

Stats creates a Docker producer only while subscribers exist and retains only the latest sample, never an unbounded history. The final subscriber cancels the producer. A separate producer registry ensures deletion and shutdown can cancel and join every generation.

The embedded asset handler serves static files, SPA fallback, and security headers. `/api/**` and `/hooks/**` never enter SPA fallback. See [webhooks](webhooks.md) for the public webhook's separate Host and signature protocol.
