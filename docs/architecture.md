# SoloDock architecture

> English (authoritative) · [简体中文](zh-CN/architecture.md)

SoloDock is a single-host, single-administrator, single-service container deployment control plane. It keeps one Rust process, one crate, and one Docker mutation path; it does not introduce internal microservices, a general workflow engine, or a second Compose specification.

```text
Browser
  -> external tunnel / WAF / TLS
  -> loopback SoloDock process
       |-- REST + bounded SSE + embedded UI
       |-- private filesystem app/release/credential/secret store
       |-- SQLite auth/audit/idempotency/deployment/poll ledger
       |-- Docker Engine API for observation
       |-- fixed docker compose CLI for exact mutation
       `-- OCI Registry client for tag-to-digest resolution
```

See [product scope](product-scope.md) for capabilities and non-goals, and [application model](application-model.md) for supported configuration and resources.

## Component responsibilities

- Axum API: authentication, typed DTOs, mutation coordination, SSE, and the embedded frontend.
- Application store: application metadata, immutable config revisions, releases, active/pending links, and webhook secrets.
- SQLite: authentication, sessions, audit, idempotency, deployment transitions, and poll/replay operational state.
- Docker observer: capability, list/inspect/events/logs/stats, and ownership over a fixed socket.
- Compose adapter: generates canonical single-service YAML from typed configuration and executes a closed set of fixed argument vectors.
- Registry adapter: canonical references, Bearer authentication, and manifest/digest/platform resolution.
- Deployment engine: the only scheduler and execution state machine for manual deployment, polling, and rollback.
- `PollCoordinator`: bounded due heap, backoff, coalescing, and durable webhook wake.
- Projection/reconciliation: refreshes catalog, redactor, and SQLite query projections from complete filesystem facts.

The production binary embeds Vite output at compile time with `embed-ui`; no Node service runs. Hashed assets may use long-lived caching, while HTML is uncached. `/api/**` and `/hooks/**` never enter SPA fallback. See [API and streams](api-and-streams.md).

## Sources of truth

| Fact | Authoritative source |
| --- | --- |
| Application metadata, draft config, managed files, credential references | Private filesystem |
| Registry/webhook/application secret plaintext | Dedicated permission-constrained files |
| Immutable releases, digest, platform, canonical Compose | Private filesystem |
| Active/pending release | Canonical symlink in the application directory |
| Actual container state, full ID, image, and resource existence | Docker daemon |
| Current tag target | Registry; after scheduling, the release manifest digest |
| Administrator, sessions, audit, idempotency, deployment execution, and poll/replay state | SQLite |
| Global display timezone and allowed bind roots | SQLite global settings |
| Catalog, redactor, and query indexes | Rebuildable projections from the facts above |

No SQLite projection may overwrite filesystem facts. If SQLite is lost, application and release query facts can be reconstructed from files, but administrator credentials, sessions, audit, and deployment history cannot be fabricated; bootstrap must run again.

The application header persists UUID, immutable slug, and `resource_name_schema_version`, but not the derived project name. The domain naming helper is the sole source of project, owned network, owned volume, and bridge names. Old schemas retain slug-based bridges; new schemas use UUID-token bridges. An asynchronous path holding only a UUID must reload validated metadata/catalog rather than guessing a resource name.

## Persistent layout

The default roots are below. Startup and recovery validate permissions, HMACs, and canonical entries:

```text
/etc/solodock/config.toml

/var/lib/solodock/
  state.sqlite3
  secrets/idempotency.key
  registry-credentials/<credential-id>/
    credential.toml
    secret-revisions/<revision-id>/token
  registry-credentials/.trash/<credential-id>-<operation-id>/
  apps/<app-id>/
    app.toml
    webhook.toml
    webhook-secret-revisions/<revision-id>/
    config-revisions/<revision-id>/
      config.toml
      env/
      files/
    releases/<release-id>/
      release.toml
      compose.yaml
    active -> releases/<release-id>
    pending -> releases/<release-id>

/run/solodock/
  bootstrap.token
  locks/<app-id>.lock
  compose/<operation-id>/
  docker-config/<deployment-id>/config.json
```

Exact filenames may evolve through compatible migrations. Callers must not bypass the store or recovery logic to edit these artifacts directly.

## Filesystem-first publication

Config revisions and releases are written to operation-owned temporary locations under the same parent. They become referenceable only after file `fsync`, atomic rename, and parent `fsync`. Application metadata or the `active`/`pending` link is the corresponding visibility commit point.

In-memory catalog/redactor and SQLite projections are published only after the filesystem commit. Projection failure cannot report already committed facts as rolled back; the system becomes degraded and a shutdown-aware reconciler retries. The redactor may destructively replace its patterns only after a complete inventory of every application, active/pending/draft, Registry credential, and webhook secret. An incomplete inventory preserves old patterns or prevents cold startup.

Destructive recovery cleanup runs only before the HTTP listener starts. Runtime verified loaders, catalog refresh, and reconciliation use read-only scans; they cannot delete a concurrent writer's temporary artifact or a new revision not yet referenced by old metadata.

Replay retention and recovery-proof retention are separate lifecycles. Terminal idempotency responses normally expire after 24 hours, while an exact proof remains protected for as long as an application/credential tombstone, webhook revision, or webhook operation temporary directory depends on it. Webhook inventory authenticates every canonical revision, including the current one. Stale revision cleanup is authorized by the successful transition named by the current signed `webhook.toml`, with its recorded response matched to the current metadata identity. The global mutation coordinator serializes artifact publication/finalization with fresh inventory through the bounded SQLite deletion commit. An incomplete inventory or unverifiable historical proof deletes nothing.

## Docker and Compose boundary

Production observation uses only `/var/run/docker.sock` and ignores `DOCKER_HOST`. If Docker is unavailable, the authenticated control plane remains available and catalog/health show degraded state; streams and mutations that need Docker fail before an effect.

The production Compose runner always executes `/usr/bin/docker`, clears inherited environment, disables implicit `.env`, and never invokes a shell. It can emit only closed argument vectors for version, validate, start, recreate, deploy-candidate, stop, restart, and remove. It has no build, pull, exec, down, volume-removal, or user-argument passthrough path.

Before every effect, SoloDock reloads and verifies active/pending releases, config/HMAC/canonical YAML from the filesystem and enumerates all container candidates under the project/service. Any unmanaged, stale, invalid, replacement, or multiple collision fails closed. One canonical network plan drives Compose, resource preflight, runtime drift, deletion facts, and API projection. Active and pending expectations come from their own immutable config revisions, never the mutable draft. An unconfigured application has no revision/release, so every Docker effect ends with `APP_UNCONFIGURED` before resource creation.

An enabled owned network must match its versioned exact name and ownership, the `bridge` driver, and the bridge option. Ownership conflicts and `NETWORK_BRIDGE_IDENTITY_CONFLICT` fail before the runner. A global manager creates or validates the internal platform network before first deployment effect using fixed internal/bridge/label identity. It is neither represented as an external network nor deleted with an application. External networks use fresh network inspection plus bounded-concurrency member-container inspection to obtain full IDs and effective DNS names. Missing networks, alias conflicts, or incomplete observation fail before the runner. Resources, networks, binds, and daemon data root are rechecked after the durable marker; the runner is called only after the final external fact check.

A built-in preset only renders a small input set to ordinary `DraftInput`; it does not generate Compose or manipulate Docker. OCI metadata inspection is likewise a side-effect-free reader that reuses Registry authentication, redirect, timeout, and digest checks and returns only an allowlist for the UI. Both paths return to the sole config revision, release resolver, and deployment engine.

## Releases and automation

Manual, poll, and rollback work enter one deployment engine. The candidate is persisted and `pending` is set before a Docker effect. The first observation after Compose is the ownership-claim boundary: one non-predecessor full ID with the complete canonical project/service/application/schema/candidate-release labels proves an exact owned effect. The worker immediately persists that ID as `post_container_id`, then validates configured digest reference, config/manifest identity, available manifest descriptor, status, and health. Once persisted, that exact ID is the source of truth for compensation, health, and commit/rollback. Any later different full ID is an uncertain replacement, so pending and the scene remain and the deployment becomes `interrupted` or `needs_attention`; SoloDock must not stop or remove it. Deterministic rejection of an exact owned candidate enters the same remove-or-restore compensation path, and only newly observed proof of compensation permits `failed` or `rolled_back`.

After webhook HMAC verification, nonce claim, audit, and per-application wake sequence commit in one SQLite transaction. The sequence is only a bounded coalescing signal; `PollCoordinator` still reloads filesystem, Registry, and Docker facts. See [deployments and rollback](deployments.md) and [webhooks](webhooks.md).

## Data and recovery boundary

Unregister, remove, and delete preserve named/external volumes, bind contents, and networks. Business data is outside the control-plane backup, and release rollback does not reverse data migrations. See [operations](operations.md), [recovery](recovery.md), and the [threat model](threat-model.md).
