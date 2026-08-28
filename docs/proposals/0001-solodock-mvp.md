# SoloDock MVP Proposal

- Status: Proposed
- Date: 2026-08-28
- Target: Ubuntu 24.04, one host, one administrator
- Repository: `baixiaohang/solodock`

## 1. Summary

SoloDock is a lightweight, open-source deployment console for personal, single-host Docker workloads. It deploys prebuilt container images, exposes a small Web UI, resolves mutable image tags to immutable digests, checks application health, and rolls back failed releases.

SoloDock is not a general Docker administration suite or a full PaaS. It does not build source code, manage domains or TLS, provide a reverse proxy, or orchestrate multiple hosts. Its intended deployment sits behind an existing Cloudflare Tunnel and IP allowlist and listens only on host loopback.

The MVP deliberately uses a narrow application model:

```text
one SoloDock application = one Docker service/container = one prebuilt image
```

SoloDock owns and generates a minimal Compose file for each application. Users configure the supported fields through the UI; the MVP does not import arbitrary existing Compose projects and does not expose a raw Compose editor.

## 2. Context and Product Comparison

The target host is an Ubuntu 24.04 Tencent Cloud server with 2 vCPU and 4 GiB RAM. It already runs Docker Compose, Cloudflare Tunnel, SoloGrove, PostgreSQL, and other applications. The control plane must leave most resources available to those workloads.

| Product | Strength | Why it is not the target |
| --- | --- | --- |
| CapRover | Complete deployment UX, image/source deployment, health checks, proxy, TLS, and clustering | Docker Swarm, Nginx, Let's Encrypt, source builds, and cluster features exceed the required scope. Its Compose support is a limited subset. |
| Dockge | Lightweight, file-oriented Compose UI with lifecycle controls and logs | It focuses on manual Compose management rather than immutable releases, automated digest discovery, health-gated deployment history, and rollback. |
| Portainer | Broad Docker, Swarm, Kubernetes, registry, and infrastructure management | It is much broader than application delivery; some GitOps/webhook behavior depends on the product edition. |
| Dokku | Mature Heroku-style CLI deployment and health-check model | It is centered on Git/source builds, plugins, proxies, and CLI workflows rather than a small image-only Web console. |
| Coolify | Full self-hosted PaaS with Git integrations, builds, proxies, services, and rollback | Its control plane and local build surface are too broad for a busy 2C4G host. Official guidance starts at 2 cores and 2 GiB RAM. |
| Dokploy | Full deployment platform with Compose/Swarm, providers, builds, Traefik, monitoring, and backups | It includes many capabilities that SoloDock explicitly excludes. |

References:

- [CapRover](https://caprover.com/)
- [CapRover Docker Compose support](https://caprover.com/docs/docker-compose)
- [Dockge](https://github.com/louislam/dockge)
- [Portainer webhooks](https://docs.portainer.io/user/kubernetes/applications/webhooks)
- [Dokku architecture](https://dokku.com/docs/development/architecture/)
- [Coolify installation requirements](https://coolify.io/docs/get-started/installation)
- [Coolify Docker Compose model](https://coolify.io/docs/knowledge-base/docker/compose)
- [Dokploy Docker Compose](https://docs.dokploy.com/docs/core/docker-compose)

## 3. Goals

The MVP must:

- run as a low-overhead Rust service;
- support one host and one administrator;
- provide a Web management UI;
- create and manage multiple single-service applications;
- accept prebuilt OCI/Docker image references from GHCR, Docker Hub, and compatible registries;
- support private GHCR pull credentials;
- configure environment variables, mounted configuration files, ports, volumes, networks, and one container health policy per application;
- start, stop, restart, deploy, unregister, and remove an application's containers without deleting its volumes;
- show container state, bounded logs, and live CPU/memory/network statistics;
- poll a configured image tag and resolve it to an immutable digest;
- deploy only by immutable digest;
- wait for the configured health policy and automatically restore the previous digest after a normal failed deployment;
- keep deployment history and support manual rollback;
- reject concurrent mutation of the same application;
- make deployment retryable after a host or process interruption;
- keep application configuration and release snapshots recoverable from files;
- listen only on `127.0.0.1`, with public access supplied externally by Cloudflare Tunnel.

## 4. Non-goals

The MVP does not:

- manage Nginx, Traefik, domains, DNS, TLS, or Cloudflare configuration;
- support Docker Swarm, Kubernetes, multiple hosts, high availability, multi-tenancy, or RBAC;
- clone repositories, build Dockerfiles, run buildpacks, or execute `docker compose build`;
- accept a GitHub source repository URL as a deployable input; the repository must publish a container image first;
- support multiple services or multiple replicas inside one SoloDock application;
- import, scan, or take over an arbitrary existing Compose directory;
- expose an arbitrary Compose YAML editor;
- provide a browser shell, host command runner, container exec terminal, or custom Compose flags;
- automatically modify, back up, restore, prune, or delete existing volumes;
- promise zero-downtime deployment;
- roll back database schemas or persistent data;
- automatically resume a deployment at the exact phase where power was lost;
- verify Cosign/Sigstore signatures in the MVP.

## 5. Application and Configuration Model

### 5.1 Image input

An application accepts exactly one image reference, for example:

```text
ghcr.io/baixiaohang/sologrove:staging
ghcr.io/example/private-api:v1
postgres:16-alpine
docker.io/dpage/pgadmin4:latest
```

A mutable tag is a discovery input only. A release always records and runs a reference of the form:

```text
registry.example/namespace/image@sha256:<digest>
```

For a multi-platform image, SoloDock records the registry index/manifest digest, selected platform, and local Docker image ID so the release remains explainable.

### 5.2 Environment variables

The UI offers two views over one canonical environment-variable set:

- Table view: one key/value row at a time, with an explicit secret flag.
- Bulk view: parse and validate standard `.env` input, reject duplicate or invalid keys, and show a diff before saving.

The views must not maintain independent copies. Non-secret values can round-trip between both views. Secret values are accepted on write but never returned by the API. A masked placeholder must never overwrite a stored secret; bulk updates require explicit keep, replace, or delete semantics.

The generated Compose file contains variable references, not secret values. Public values and secret values are stored in separate permission-restricted files.

### 5.3 Mounted configuration files

An application can own bounded text configuration files such as JSON, YAML, dotenv, certificates, or PEM material. Each entry defines:

- a logical name;
- a container target path;
- whether it is sensitive;
- read-only mounting, which is mandatory in the MVP;
- content size and total application quota.

Normal files and secret files use separate host directories and permissions. Secret content is write-only through the API after initial submission. Paths are canonicalized and cannot target the Docker socket, host root, or other sensitive host paths.

### 5.4 Ports, volumes, and networks

- Published ports default to explicit loopback bindings such as `127.0.0.1:8000:8000`.
- Binding to a non-loopback host address is rejected in the MVP.
- SoloDock can create application-owned named volumes and attach explicit existing named volumes.
- Existing volumes are treated as external and never modified or deleted.
- Application removal never passes `-v`/`--volumes` to Compose.
- Each application receives an isolated default network and can attach explicit existing external networks for shared PostgreSQL or other dependencies.
- Removing an application never deletes an external network.

### 5.5 Health policy

Each application selects one of:

- `healthy`: the image or generated Compose service must define a Docker health check and reach `healthy`;
- `running`: the container must remain running for a stability window, defaulting to 15 seconds;
- `completed`: reserved for explicitly configured one-shot applications and requires exit code 0;
- `disabled`: permitted only with a visible warning; automatic rollback can detect only startup failure.

The UI may provide an HTTP health-check form that generates a Docker health check, but it must explain that the required command must exist in the image. A built-in image health check takes precedence unless the user explicitly replaces it.

## 6. Architecture

```text
Browser
  -> fixed VPN/TUN egress IP
  -> Cloudflare WAF allowlist
  -> Cloudflare Tunnel
  -> 127.0.0.1:<port>
  -> SoloDock (Rust)
       |-- REST + SSE API
       |-- filesystem application/release store
       |-- SQLite operational ledger
       |-- Docker Engine API via Bollard (observation)
       |-- docker compose CLI (exact application mutations)
       `-- OCI Registry V2 / GHCR digest resolver
```

SoloDock is a single Rust process and a single Rust crate. It does not introduce internal services, a plugin system, or a generic workflow engine.

### 6.1 Technology choices

- Rust stable, edition 2024.
- Axum and Tokio for HTTP, middleware, streaming, and task coordination.
- Server-Sent Events instead of WebSocket in the MVP because state, logs, statistics, and deployment progress are server-to-client streams. SSE provides reconnect semantics with less protocol state.
- Bollard for Docker list, inspect, events, stats, logs, and image inspection.
- Official Docker Compose CLI for generated project validation and lifecycle mutation. SoloDock never invokes a shell and builds a fixed argument vector.
- SQLx with SQLite in WAL mode for sessions, deployment jobs, idempotency records, audit events, and query indexes.
- Serde/TOML for owned configuration. YAML generation is limited to SoloDock's small Compose schema; official Compose validation remains authoritative.
- Reqwest-based OCI Distribution adapter for manifest digest resolution and bearer-token authentication, initially tested against GHCR and Docker Hub.
- Svelte, TypeScript, and Vite for the static frontend. The final release embeds built assets in the Rust binary.
- Tracing for structured logs; Argon2id for the administrator password; secrecy/zeroize-style wrappers for sensitive values.

### 6.2 Source-of-truth boundaries

Each fact has one authoritative owner:

- Application configuration, public environment data, mounted files, credential references, and immutable release snapshots: filesystem.
- Secret values: dedicated permission-restricted files; never duplicated into Compose or SQLite.
- Actual container state: Docker daemon.
- Sessions, deployment execution state, idempotency keys, audit events, and query indexes: SQLite.
- Current mutable tag digest: registry; once a release is created, the release digest is authoritative for that release.

SQLite loss must not prevent scanning application directories to recover applications, the active release, generated Compose, and image digests. Historical audit entries are not reconstructed or fabricated after database loss.

Critical file writes use a same-directory temporary file, file `fsync`, atomic rename, and parent-directory `fsync`. Release directories are immutable after creation.

## 7. Host Storage Layout

```text
/etc/solodock/config.toml

/var/lib/solodock/
  state.sqlite3
  credentials/<credential-id>.json
  apps/<app-id>/
    app.toml
    env/public.env
    secrets/runtime.env
    files/public/<name>
    files/secret/<name>
    releases/<release-id>/
      release.toml
      compose.yaml
    active -> releases/<release-id>
    pending -> releases/<release-id>          # present only during/retry after deployment

/run/solodock/
  locks/<app-id>.lock
  docker-config/<operation-id>/config.json
```

Expected modes are enforced at startup. Runtime registry authentication is materialized in an operation-scoped `DOCKER_CONFIG` directory under `/run`, never passed in command arguments, and removed after use.

## 8. Core Data Model

### App

```text
id, slug, display_name, project_name
discovery_image_ref, credential_ref
desired_state, auto_deploy_enabled, poll_interval
ports[], volumes[], networks[]
health_policy
active_release_id
schema_version, created_at, updated_at
```

`project_name` is generated once, immutable, and passed explicitly to every Compose command. A user-supplied slug is never used directly as an unvalidated CLI argument.

### Release

```text
id, app_id, config_sha256
source_image_ref, resolved_digest, platform, local_image_id
compose_snapshot_path
trigger_metadata, created_at
```

A release is immutable. Manual rollback creates a new deployment whose candidate points to an older release; it never mutates historical records.

### Deployment

```text
id, app_id
trigger: manual | poll | rollback | config
from_release_id, candidate_release_id
status, phase, idempotency_key
error_class, error_code, redacted_message
started_at, completed_at
```

### RegistryCredential

```text
id, registry, username, secret_file_ref
created_at, rotated_at
```

The API returns credential metadata only. GHCR documentation must recommend a dedicated classic PAT with only `read:packages` where possible.

### AuditEvent

```text
id, actor, request_id, action
target_type, target_id, result
redacted_metadata, created_at
```

Container status and metrics are derived from Docker and are not persisted as competing business state.

## 9. Deployment State Machine

```text
QUEUED
  -> RESOLVING      tag -> digest; same digest becomes a recorded no-op
  -> PREPARING      validate configuration and atomically write candidate release
  -> PULLING        pull only image@digest
  -> APPLYING       run generated Compose for the immutable candidate
  -> VERIFYING      wait for health policy and stability window
  -> COMMITTING     atomically switch active release and commit DB status
  -> SUCCEEDED

Failure before APPLYING -> FAILED, runtime unchanged
Normal failure after APPLYING -> ROLLING_BACK -> VERIFYING_ROLLBACK
                                           |-> ROLLED_BACK
                                           `-> NEEDS_ATTENTION

Host/process interruption -> INTERRUPTED + DRIFTED if actual != active
Next manual or polling deployment -> execute target candidate from the beginning
```

### 9.1 Concurrency

- SQLite atomically claims an application mutation and an advisory application file lock provides a second process boundary.
- A concurrent mutation returns `409 APP_BUSY`.
- One global deployment semaphore defaults to one, limiting image pull and extraction pressure on the 2C4G host.
- Polling does not build an unbounded queue. A busy application is checked again on the next interval, naturally coalescing intermediate tag changes.

### 9.2 Interruption model

The MVP does not automatically infer and resume an exact interrupted phase.

- The candidate release is durable before Docker mutation.
- The active release is unchanged until successful verification.
- On startup, non-terminal jobs become `INTERRUPTED`.
- SoloDock compares the active expected digest with the actual container digest and displays drift.
- Start, restart, and configuration mutation are blocked while unresolved drift exists.
- The next manual Deploy or registry-poll deployment runs the target candidate from the beginning. Compose operations are idempotent and converge the single container to that release.
- If the retried candidate fails normally, SoloDock restores and verifies the active release.

Docker's configured restart policy remains responsible for starting an already-created container after host reboot. SoloDock does not make speculative cleanup changes on startup.

### 9.3 Rollback boundary

Rollback restores the generated Compose configuration and immutable image digest only. It cannot undo database migrations, writes to bind mounts, or named-volume contents. Applications with irreversible migrations must use backward-compatible migration patterns or disable automatic rollback after an explicit warning.

## 10. Registry Polling and Optional Webhook

Polling is the primary automatic-deployment mechanism:

1. Resolve the configured tag through the registry manifest endpoint with the required OCI/Docker media types.
2. Follow bearer-token authentication without exposing credentials.
3. Compare the returned digest with the active release digest.
4. If it differs and the application is idle, begin a deployment by digest.
5. Apply jitter and bounded exponential backoff to registry/transient errors.

Default polling interval is five minutes and is configurable per application within safe limits.

A signed deployment webhook is deferred until after the core MVP. If added, it must use a separate hostname and exact WAF path/method rule, HMAC-SHA256, timestamp window, nonce replay protection, and body limits. The webhook is only a prompt to re-query the registry; request content never becomes a trusted Docker image argument.

## 11. Authentication and Threat Model

### 11.1 Deployment boundary

The recommended production path follows the existing SoloGrove staging posture:

```text
Browser -> fixed VPN/TUN egress IP -> Cloudflare WAF allowlist
        -> Cloudflare Tunnel -> 127.0.0.1:<SoloDock port>
```

- Tencent Cloud security groups and host firewall expose no SoloDock HTTP/HTTPS port, for either IPv4 or IPv6.
- `cloudflared` is the only public path and establishes an outbound tunnel.
- Cloudflare WAF is the first filtering layer but does not replace application authentication.
- SoloDock still requires its own administrator password.
- Cloudflare Access/MFA is optional for the fixed-IP, single-admin MVP. It becomes recommended if the IP allowlist is removed, mobile access is required, or additional administrators are introduced.

### 11.2 Application authentication

- One administrator account.
- Initial password is established with a one-time loopback bootstrap token, not by the first public visitor.
- Strong unique password stored as an Argon2id hash.
- Secure, HttpOnly, SameSite=Strict session cookie.
- CSRF token and exact Origin validation for mutations.
- Login throttling/cooldown, bounded session lifetime, and revoke-all-sessions support.
- Successful/failed login and sensitive operations are audited without recording secrets or cookies.

HTTP Basic authentication is not used.

### 11.3 Docker socket boundary

SoloDock runs as a dedicated non-login system user, not UID 0:

```ini
User=solodock
Group=solodock
SupplementaryGroups=docker
```

However, membership in the Docker group and access to `/var/run/docker.sock` are effectively root-equivalent. A compromised SoloDock process can ask Docker to mount host root or start privileged workloads. Non-root UID reduces ordinary file-permission mistakes but is not a privilege boundary against Docker daemon access.

Mitigations:

- never expose the Docker socket through the Web API or to managed application containers;
- never enable unauthenticated Docker TCP access;
- expose only fixed lifecycle actions and structured configuration fields;
- use exact Compose project name, Docker labels, and actual object IDs before destructive operations;
- never invoke a shell or accept arbitrary command arguments;
- keep application and registry secrets out of logs, errors, audit metadata, Compose files, process arguments, and normal API responses;
- bound log lines, stream buffers, rates, and concurrent connections;
- lint and visibly warn on root users, privileged mode, host namespaces, devices, or Docker socket mounts. The MVP's structured model should reject features it does not support.

Host root compromise, Docker daemon compromise, and a malicious sole administrator are outside the achievable same-host threat boundary.

## 12. API Sketch

All mutation endpoints accept `Idempotency-Key`. Errors use a common shape:

```json
{
  "code": "APP_BUSY",
  "message": "The application already has an active mutation",
  "request_id": "..."
}
```

No error details include secrets.

```text
POST   /api/v1/auth/bootstrap
POST   /api/v1/auth/login
POST   /api/v1/auth/logout
GET    /api/v1/me
POST   /api/v1/me/sessions/revoke-all

GET    /api/v1/apps
POST   /api/v1/apps
GET    /api/v1/apps/{id}
PUT    /api/v1/apps/{id}/draft
POST   /api/v1/apps/{id}/validate
POST   /api/v1/apps/{id}/deployments
POST   /api/v1/apps/{id}/actions/start
POST   /api/v1/apps/{id}/actions/stop
POST   /api/v1/apps/{id}/actions/restart
POST   /api/v1/apps/{id}/deletion-preview
DELETE /api/v1/apps/{id}

GET    /api/v1/apps/{id}/deployments
GET    /api/v1/deployments/{id}
POST   /api/v1/deployments/{id}/rollback

GET    /api/v1/apps/{id}/events
GET    /api/v1/apps/{id}/logs
GET    /api/v1/apps/{id}/stats

GET    /api/v1/registry-credentials
POST   /api/v1/registry-credentials
PUT    /api/v1/registry-credentials/{id}
DELETE /api/v1/registry-credentials/{id}

GET    /api/v1/system/health
GET    /api/v1/system/drift
```

The events, logs, and stats endpoints are bounded SSE streams. Logs accept limited service-independent filters such as tail count and since time; there is no shell or exec endpoint.

Destructive deletion is a two-step operation. The preview returns the exact project, container, network, owned files, and retained volumes plus a short-lived confirmation token. The delete request includes that token and the application slug. Unregister-only is the default; container removal is an explicit option and still preserves volumes.

## 13. UI Sketch

```text
+ Dashboard ------------------------------------------------+
| Docker OK | disk 61% | active deployment: none           |
| [New application]                                        |
|                                                          |
| SoloGrove  healthy  sha256:ab...  CPU 3%  RAM 420 MiB   |
| insight    running  sha256:cd...  CPU 1%  RAM 110 MiB   |
| pgAdmin    stopped  [Start]                              |
+----------------------------------------------------------+

Application / SoloGrove
  Overview | Configuration | Deployments | Logs | Settings
```

- Dashboard: Docker/system health, disk pressure, deployment activity, and app cards.
- New application: image reference, credential, environment, files, ports, volumes, networks, health, and auto-deploy policy; validation and exact preview before creation.
- Overview: actual versus active digest, container status, ports, mounts, networks, live resource summary, and fixed lifecycle actions.
- Configuration: table/bulk environment editor, write-only secrets, mounted files, and preview-before-deploy.
- Deployments: trigger, source tag, immutable digest, phase, duration, health result, error class, and rollback relationship.
- Logs: bounded tail/stream, pause, and download-current-window; no terminal.
- Settings: polling, registry credential reference, warnings, unregister, and deletion preview.

## 14. Planned Repository Layout

```text
Cargo.toml
Cargo.lock
rust-toolchain.toml
src/
  main.rs
  config.rs
  domain/
  api/
  auth/
  app_store/
  db/
  docker/
  compose/
  deploy/
  registry/
  security/
  telemetry/
web/
  package.json
  src/
migrations/
tests/
  fixtures/
  integration/
  e2e/
packaging/
  systemd/solodock.service
docs/
  proposals/
  operations.md
  recovery.md
.github/workflows/
```

The project stays one Rust crate until a real independently reusable or releasable boundary appears.

## 15. Delivery Plan

Each task is independently testable. Exact implementation paths may be refined without changing the boundaries above.

### M0: Repository and executable skeleton

- Create the Rust/Axum and Svelte/Vite skeleton, formatting, linting, tests, CI, Apache-2.0 license, and development README.
- Add a loopback-default server with `GET /healthz` and graceful shutdown.
- Add frontend type-check/build scripts and a minimal page.
- Add the systemd packaging skeleton for the dedicated `solodock` user.

Paths: `Cargo.toml`, `src/`, `web/`, `.github/workflows/`, `packaging/`, root metadata.

Verification: clean Rust format/clippy/test, frontend check/build, and CI; server refuses or ignores non-loopback public binding according to the bootstrap scope.

### M1: Durable store and authentication

- Implement host configuration, atomic application/release files, SQLite migrations, recovery scan, structured errors, and tracing.
- Implement one-time local bootstrap, login/session/CSRF, loopback enforcement, and login audit.

Paths: `src/config.rs`, `src/app_store/`, `src/db/`, `src/auth/`, `src/api/auth.rs`, `migrations/`.

Tests: atomic-write recovery, permission enforcement, DB index rebuild, authentication replay, CSRF, session revocation, and non-loopback rejection.

### M2: Read-only Docker console

- Implement Docker capability probing and exact application discovery through labels.
- Implement container state, bounded logs, on-demand stats, and events through Bollard.
- Implement Dashboard and Overview/Logs pages.

Paths: `src/docker/`, `src/api/streams.rs`, `web/src/`.

Tests: fake Docker unit tests plus isolated-daemon integration tests, slow clients, reconnect, rate/buffer limits, and secret canaries.

### M3: Managed single-service lifecycle

- Implement the structured application schema and minimal generated Compose adapter.
- Implement environment table/bulk parsing, mounted normal/secret files, loopback ports, volumes, networks, and health policy.
- Implement validation/preview and exact create/start/stop/restart/unregister/remove actions.

Paths: `src/domain/`, `src/compose/`, `src/security/`, `src/api/apps.rs`, configuration UI.

Tests: injection and path traversal, duplicate env keys, write-only secrets, external resources, project collision, command timeout, and proof that deletion never uses `-v` or removes external resources.

### M4: Digest releases and rollback

- Implement GHCR/Docker Hub registry digest resolution and scoped temporary Docker authentication.
- Implement immutable release creation, application/global locks, deployment state machine, health gate, automatic rollback, manual rollback, and interrupted/drift detection.
- Implement deployment history/detail UI.

Paths: `src/registry/`, `src/deploy/`, `src/app_store/releases.rs`, deployment UI.

Tests: public/private registry, 401/403, manifest lists, tag race, exact digest deployment, concurrent `409`, every normal failure phase, health failure rollback, rollback failure, and retry after interruption.

### M5: Automatic deployment and production hardening

- Implement jittered registry polling and no-op/coalescing behavior.
- Embed static frontend assets in the Rust release binary.
- Complete systemd installation, upgrade, backup, recovery, threat-model, and operations documentation.
- Benchmark idle/stream/deploy resources on the target class of host.
- Write one-time migration runbooks for pgAdmin, insight-agent, and SoloGrove. These are operational procedures, not a generic import feature.

Paths: `src/registry/poller.rs`, packaging, `docs/`, release workflow, E2E suites.

Tests: polling errors/backoff, same-digest no-op, busy-app coalescing, installation smoke test, static-asset serving, resource budgets, and the full isolated deployment/rollback flow.

## 16. Test Strategy and Safety

### 16.1 Layers

- Pure unit tests use traits/fakes for Docker, Compose, registry, clock, and filesystem boundaries.
- Integration tests exercise SQLite and atomic filesystem behavior in temporary directories.
- CI Docker E2E uses a dedicated Docker-in-Docker daemon and never mounts the CI host Docker socket into the test control plane.
- Server acceptance tests require an explicit test Docker context or explicit opt-in. They create only random `solodock-test-<uuid>` projects with run-token labels.

### 16.2 Destructive-test guardrails

- Cleanup uses the exact IDs recorded by the current test run.
- Cleanup first verifies project prefix, test labels, and run token.
- No test runs `docker system prune`, wildcard removal, global image cleanup, or `compose down -v`.
- No test scans and removes objects it did not create.
- Existing SoloGrove, PostgreSQL, insight-agent, pgAdmin, networks, and volumes are outside every test selector.

### 16.3 MVP acceptance criteria

- A tag resolves to a digest and the running container uses `image@sha256:...` even if the tag changes during deployment.
- Two concurrent mutations for one application produce exactly one claim and one `409 APP_BUSY`.
- A normal unhealthy candidate restores and verifies the previous digest without deleting or replacing volumes.
- An interrupted deployment is marked interrupted/drifted; the next deployment converges to the selected release and completes or performs the normal rollback path.
- Environment table and bulk views share one canonical value set; duplicate keys are rejected.
- A secret canary does not appear in normal API responses, SSE, audit rows, tracing, errors, Compose files, release files, or CLI arguments.
- Deletion defaults to unregister. Explicit container removal preserves all named/external volumes and external networks.
- Loss of an exercise copy of SQLite still permits recovery of applications, active releases, generated Compose files, and image digests from the filesystem.
- Management HTTP binds only to loopback and there is no shell/exec endpoint.
- Docker E2E cleanup cannot match pre-existing host applications or data.

## 17. Resource Budget and Host Operation

These are design budgets to be measured during M5, not claims about an unimplemented binary.

| Resource | Target budget |
| --- | --- |
| Rust control plane plus embedded UI, idle RSS | 40–100 MiB |
| Idle CPU | Normally below 1%, excluding polls/events |
| Active UI streams | Additional 10–40 MiB within hard connection/buffer limits |
| Compose/pull transient client memory | Roughly 100–300 MiB; Docker daemon extraction can use more |
| SoloDock binary/UI/metadata | Tens of MiB for binary/assets; metadata target below 100 MiB excluding Docker image layers |

Operational defaults:

- native systemd service, not another control-plane container;
- dedicated non-login `solodock` user with Docker supplementary group;
- `127.0.0.1` listener only;
- `UMask=0077`, explicit read/write paths, restart on failure, task/file-descriptor limits;
- `MemoryHigh` near 256 MiB initially, with a hard limit only after measurement so rollback is not killed under pressure;
- one deployment globally;
- five-minute registry polling with jitter;
- Docker stats sampled only while a UI subscriber exists;
- disk-space reporting and warnings, but no automatic Docker pruning.

Image pull/extraction and application restart are more likely to pressure the 2C4G host than SoloDock's idle process. Serial deployment, no local builds, and pre-deployment disk/memory checks are therefore core requirements.

## 18. Primary Risks

| Risk | Impact | Mitigation |
| --- | --- | --- |
| In-place single-container replacement | Short downtime | Explicit non-goal; use health gate and fast digest rollback. |
| Irreversible application migration | Old image may not work with changed data | Strong warning, optional automatic rollback disable, expand/contract migration guidance, independent backups. |
| Docker socket compromise | Host root-equivalent impact | Narrow authenticated loopback API, fixed actions, no shell, exact targets, WAF/Tunnel/password layers, security testing. |
| Compose CLI version differences | Mutation behavior or flag mismatch | Startup capability probe and minimum supported version; disable mutations while retaining read-only observation if unsupported. |
| Registry outage/rate limit | Delayed automatic deployment | Conditional requests where available, jitter, bounded backoff, specific error classes; current release remains active. |
| Multi-platform manifest confusion | Incorrect audit/recovery information | Record index/manifest digest, platform, and local image ID; test amd64 and arm64 fixtures. |
| Secret included in third-party output | Credential disclosure | Do not use argv, minimize captured output, central redaction, canary tests, structured error summaries only. |
| SQLite or disk damage | Lost execution history or state | WAL/checkpoint, backup docs, filesystem recovery of apps/releases, disk preflight; never fabricate audit history. |
| Existing-app migration error | Downtime or wrong volume attachment | Application-specific maintenance runbook, exact preview, explicit existing volume/network names, and no generic automatic takeover. |

## 19. Architecture Principles Check

- **Single source of truth:** Files own desired application/release facts, Docker owns actual runtime state, and SQLite owns operational history. The table and bulk environment editors are two projections over the same data.
- **Replace, do not coexist:** SoloDock does not implement a second Compose specification. It generates a narrow schema and delegates authoritative validation/execution to the installed Compose CLI.
- **Right-sized abstraction:** One process, one Rust crate, one application service, SQLite, SSE, and one global deployment avoid platform abstractions that the personal single-host use case does not need.
- **Specific failure and atomic action:** Registry, credential, deterministic configuration, health, host resource, and interruption errors are distinct. Candidate persistence precedes Docker mutation; active switching follows health verification.
- **Blast-radius audit:** Image digest semantics apply to deploy/start/restart/rollback/drift/UI. Deletion previews containers, files, networks, and retained volumes before any mutation.
