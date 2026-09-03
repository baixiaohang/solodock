# SoloDock testing and safety guardrails

> English (authoritative) · [简体中文](zh-CN/testing.md)

Testing must prove more than the happy path. It must show that a Docker-root control plane still fails closed during failure, interruption, ownership drift, and data-retention scenarios.

## Layers

- Unit tests: domain validation, reference/parser, HMAC, redactor, state machine, clock/backoff, paths, and filesystem helpers.
- Integration/API: temporary-directory SQLite, filesystem-first publication, authentication, idempotency, deletion, recovery, and typed responses.
- Frontend: dotenv, write-only retry identity, deployment/poll state, credentials, row-based port/storage/health/managed-file editors, structured network editor, two-stage preset recovery, and destructive-preview components.
- Embedded/package smoke: production assets, real HTTP bootstrap/login/API, installer upgrade, systemd, and backup/restore.
- Registry + Docker E2E: a private Bearer Registry and Docker-in-Docker daemon through production HTTP, polling/webhooks, scheduler, pull, Compose, health, rollback, and cleanup boundaries.
- Resource harness: production embedded binary, 60-second idle sample, 60-second authenticated SSE, and independent dockerd sampling.

Test counts will change with the implementation. This document fixes scenarios and guardrails, not a one-run count.

Unauthenticated integration/API and Docker E2E fixtures directly insert valid, time-bounded test administrators and sessions instead of repeating production-cost Argon2. Authentication APIs still fully cover bootstrap, password hash/verify, login, cookies, CSRF, logout, revoke, and audit. Production-parameter bootstrap/login unit paths in `AuthService` also remain. Test session helpers are excluded from production binaries, and fixture bootstrap/login audit entries remain so business-case audit counts keep their meaning.

For ordinary Pull Requests, `classify` separates documentation-only from code changes. Code changes run Web checks, Rust lint, Rust tests, release/package smoke, and security-policy checks in parallel. Only recognized documentation, Web, or CI-security-tool paths may skip DinD; an unrecognized non-documentation path runs Docker E2E by default. `ci-gate` checks every classified branch and accepts only expected success or safe skips, rejecting unexpected skip, failure, or cancellation. Every `main` push runs the full checks. Security checks parse workflows structurally, validate action pins, dangerous triggers and permissions, the isolated Release attestation/publishing jobs, the fixed version-tag trigger, dependency differences, and Rust advisory/license/source policy. The tag gate also rejects a non-canonical tag or one that differs from the Cargo package version. Packaging fixtures run stable and main through the shared apply path, prove manifest-based source following and strict legacy inference, cover package/helper-only and same-binary channel transitions without a service stop, enforce the stable monotonic guard, and model GitHub Latest remaining `v0.2.0` after a newer-created `v0.1.1`. The capability-preflight fixture supplies a GitHub CLI without `gh attestation verify`, requires actionable official upgrade guidance, and proves exit before authentication, download, `sudo`, service access, or filesystem mutation. Installer failure injection exercises every staged generation asset and public-link commit point in both package-only and stopped-service paths, requiring the four public entries, unit, manifest, and API-visible identity to remain on one package after ordinary failure and retaining the forward-only pre-invocation gate. Rollback-operation injection separately fails restoration of the binary commit marker, one helper, and the unit; each must return the incomplete-rollback status, preserve the scene, and prevent `start solodock.service`. The release-generation ELF stamp gives legacy binary-only updaters a one-time safe delta into this package-aware updater. A separate workflow runs CodeQL for Rust and JavaScript/TypeScript. The classic PR suite holds SSE for 1 second to verify connections and permit release. Relevant Pull Requests keep the Docker 29 job for descriptor deployment/no-op and two compensation cases. Weekly Monday and manually triggered `Extended CI` runs the full resource window and periodically retests all three `containerd_` scenarios.

## Docker isolation

Docker/Compose E2E must use an isolated daemon or explicit test-only endpoint. Production uses fixed `/var/run/docker.sock`; only the `docker-e2e` feature may connect the runner to a test daemon.

Relevant Pull Requests run the full DinD regression against Docker 27 classic image storage and three `containerd_` scenarios against pinned Docker 29.7.2: descriptor deployment/no-op, pre-marker erroneous-claim cleanup, and post-marker replacement preservation. `Extended CI` periodically reruns the Docker 29 suite. Both daemon modes have backend assertions: the classic job rejects `io.containerd.snapshotter.v1`, while the containerd job must observe it, preventing both jobs from silently using one storage mode. The classic DinD job mounts a dedicated fixture root beneath the workspace into the daemon service at the same absolute path. Managed-file bind sources must be under that root, never a runner-temporary path invisible to the daemon, and must not mount the production Docker socket. Each `containerd_` scenario has overall, deployment/gate, and shutdown deadlines. Success, failure, panic, or timeout cleans only this scenario's containers, networks, declared volumes, and temporary bind sources by exact application/project/full ID and ownership label. The job timeout is only a last resort.

Every run generates a unique project/run token and records exact IDs for all containers, volumes, networks, and temporary bind sources. Before cleanup, re-inspect full IDs, labels, and the run token. Finally delete only objects created by that run.

Tests must never:

- mount the CI/development host's production Docker socket into the control plane under test;
- run `docker system prune`;
- delete containers, images, volumes, or networks by wildcard;
- run `docker compose down -v` or any volume-deletion option;
- scan for and delete objects lacking this run token;
- put real business services, databases, management tools, or their volumes/networks into a selector.

Bind fixtures must be within the run's private temporary root. Cleanup must not turn a successful data-retention assertion into deletion of its canary source.

## Core acceptance scenarios

### Identity and secrets

- Bootstrap at most once; Origin, CSRF, session, revoke, and heartbeat behavior.
- Public/secret classification and `keep`/`replace`/`delete`.
- Lossless row/bulk `KEY=VALUE` editing for public environment variables, including CRLF, blank lines, first `=`, duplicate/invalid keys, and line-number errors. Secrets never enter the textarea; masking, class conversion, rename, keep/replace/delete, and post-success clearing remain covered.
- Image-suggestion POST JSON `Content-Type`, CSRF, credential reference, allowlisted success projection, and sanitized error display.
- Field-level configuration `issues` locate the correct section/row without leaking public values, secrets, credentials, or host paths in responses or UI.
- Registry/webhook secret write-only behavior, zeroization, rotation/revoke, and finalization.
- A secret canary never enters API, SSE, audit, tracing, errors, Compose, releases, SQLite, or argv.
- Degraded inventory preserves the old redactor; incomplete cold-start inventory fails closed.

### Filesystem and recovery

- Temporary files, rename, parent fsync, and visible-effect failpoints.
- Runtime read-only scans never delete a concurrent writer's artifact.
- Startup-only cleanup handles only canonical ledger-owned artifacts.
- Canonical active/pending symlinks, modes/owners, HMACs, and config/release/Compose validation.
- Exact `0444` public/secret managed leaves with `0700` ancestors and publication modes that do not narrow under restrictive umask. Startup migrates only canonical legacy `0400`/`0600`; runtime scans do not change permissions and unsafe drift fails closed.
- Rebuildable facts and non-fabricated authentication/audit history after SQLite loss.
- Backup/restore rejects escaping links, hard links, special files, and incompatible state.

### Docker and Compose

- Project/service/schema/application/release/full-ID ownership.
- New immutable 1–20-character slugs, legacy 12-character boundaries, and versioned project/container/network/volume/bridge identities.
- Versioned owned-network bridge options, pre-effect identity conflicts, observer expected/actual projection, and stable identity after delete/recreate.
- `UNCONFIGURED` create/replay, first nullable revision, and deploy/start/poll/webhook failure before creating Docker resources.
- Internal platform-network inspect-or-create, same-name drift rejection, no automatic attachment of old releases, and two applications communicating by slug.
- PostgreSQL 18/17 major-specific volume targets, no secret echo, two-stage create/deploy idempotency, and partial-failure recovery.
- OCI config-blob size/media type/digest, allowlist projection, and exclusion of Env/labels/command.
- Unmanaged, stale, multiple, and replacement collisions fail before the runner.
- Canonical YAML, `.env` isolation, fixed argv, and prohibition of shell/exec/pull/build/down/volume removal.
- Canonical YAML for owned-only, owned+external, and external-only networks; byte-compatible legacy no-alias short syntax; and typed alias long syntax.
- Missing external networks, unrelated-member alias conflict, exact predecessor full-ID allowance, and fail-closed incomplete member observation.
- Immutable active/pending network expectations, attachment/alias drift, and subset semantics for Docker-added DNS names.
- Bind allowlist, symlink/device/inode/data-root revalidation, and per-row acknowledgment behavior for read-write, read-only transitions, and renewed confirmation.
- Five groups of HTTP-health numeric ranges and stability windows driven by settings capability, with Web/Rust domain agreement and fail-closed missing capability.
- English and Simplified Chinese dictionaries with compile-time key parity; locale tests cover first-visit browser detection, explicit stored preference, refresh persistence, invalid or unavailable storage fallback, immediate switching, localized timestamps, and the document `lang` attribute.
- Installation identity parsing accepts only the fixed managed symlink and canonical manifest fields; API authentication and Web presentation cover stable, main, development, unknown/failure fallback, bilingual labels, and full source/package details.
- One-time SQLite bind-root bootstrap, revision update, reference protection, and fail-closed scan errors.
- Volume/bind/network canaries survive lifecycle, deploy, rollback, unregister, and remove.
- Normal, missing, malformed, and overflow `/proc/meminfo`; the five-column system-health strip and pull gate use the same parser.
- A fixed non-root image reads public and secret managed files across initial deployment, second revision, manual rollback, and strict recovery; restart count does not increase and writes to read-only mounts fail.
- External-only configuration does not generate, inspect, or display an owned bridge identity.

### Registry and deployment

- Public/private Bearer authentication, exact scopes, and 401/403/TLS taxonomy.
- Parent/child digests, manifest media types, and canonical platform.
- Classic image-store config-ID and descriptor-absent compatibility. Docker 29.7.2 containerd storage must assert that raw `ImageInspect.Descriptor` has a digest, lacks platform, and top-level OS/architecture are complete; adapter completion then allows first deployment and same-release no-op. Bad, conflicting, or still-incomplete descriptors fail closed.
- A tag move between resolve and pull still runs the resolved digest.
- Candidate is durable before effect. The first post-effect observation claims ownership using one non-predecessor full ID and all canonical candidate-release labels and writes exact `post_container_id`.
- Shutdown grace default, `1..=600` bounds, Compose `stop_grace_period`, stop/restart argv, stop-before-remove, separate predecessor/candidate release values, and canonical hash/HMAC compatibility for legacy config/releases without the field.
- Global timezone default UTC, IANA allowlist, revision conflict, idempotent replay, Origin/CSRF/audit, and display in UTC, Asia/Shanghai, and a DST zone. API/SSE originals and expiry/cursors remain UTC.
- Deployment history has one entry per desktop row, never places two mobile entries side by side, and retains accessible detail links.
- A semantic mismatch after a pre-marker canonical-candidate claim enters deterministic compensation. Only a different full ID after the marker is a replacement; pending and the replacement container remain and state cannot pretend to be `failed`/`rolled_back`.
- On initial deployment, remove failure, observation failure after remove, or a remaining container preserves pending and original `candidate_failed` history and records only `CANDIDATE_CLEANUP_FAILED`, never `failed`.
- Health-failure automatic recovery, manual rollback, and rollback failure.
- Deterministic identity rejection after candidate creation proves removal on first deployment or restores and health-verifies the old release when active exists.
- Timeout, shutdown, and unknown effects remain interrupted and converge from fresh exact facts.
- Poll no-op, busy coalescing, backoff, ETag generation isolation, and failed-target suppression.
- Production coordinator heap/dispatch, durable webhook wake, cancellation, and `TaskTracker` join.

Full DinD acceptance also quick-deploys PostgreSQL, uses a second new application to write a canary through `<slug>:5432`, recreates PostgreSQL, and reads it again. It must not expose a PostgreSQL host port for the test. Existing host directories, UID/GID, deploy keys, and old single-writer switching for real workloads remain production maintenance-window acceptance and do not enter CI fixtures.

### Deletion

- Preview merges active/pending/draft and degraded-webhook facts while retaining external-only differences by network kind, aliases, and scope.
- Token hash is revalidated before consumption and before tombstone.
- Container candidates are checked again after slow resource inventory.
- Stream-barrier rollback/commit and producer join.
- Visible tombstone, projection failure, durable response, and background/startup finalization.

## Resource acceptance

Formal resource scenarios record commit, kernel, cgroup, toolchain, warm-up/sample windows, binary size, RSS/CPU/FD/tasks, control-plane peak, dockerd peak, and metadata size. The 8 authenticated SSE connections remain open for 60 seconds in the formal `Extended CI` window and are sampled at its end; after drop, StreamGate permits must return to zero. Ordinary Pull Requests retain only a short-window regression smoke.

See [resource budget](resource-budget.md) for targets, report format, and current baselines. These are local/CI regression baselines, not claims about a real production host.

## Documentation-change validation

At minimum, a documentation-only Pull Request runs:

```bash
git diff --check
rg -n "proposals/" README.md README.zh-CN.md docs --glob '!testing.md' --glob '!AGENTS.md'
```

Also verify manually:

- every relative Markdown link target exists;
- each English topic document has a same-named `docs/zh-CN/` translation and vice versa;
- README navigates to current topics;
- documented facts match the corresponding code, schema, and tests;
- no completed milestones, planning directories, fixed test counts, or second source of truth was reintroduced;
- the diff contains only documentation and collaboration files authorized by the task.

See repository-root `AGENTS.md` for routine development commands and default validation scope, [operations](operations.md) for operational acceptance, and [recovery](recovery.md) for restoration exercises.
