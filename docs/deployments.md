# SoloDock deployments and rollback

> English (authoritative) · [简体中文](zh-CN/deployments.md)

Manual deploy, poll auto-deploy, and rollback share the sole `DeploymentScheduler` and deployment engine. An external webhook creates only a durable Registry recheck wake; it does not establish another Registry, Docker, or Compose mutation path.

The application page reads only the latest 20 deployments. Each deployment occupies one row containing creation time in the display timezone, status/phase, trigger, image or digest, and error code, plus an explicit details link. On mobile, content within one item may wrap, but two history entries are never placed side by side.

An `UNCONFIGURED` application has no draft revision and cannot enter the scheduler, poller, webhook, or lifecycle Docker effect. It becomes deployable after the first complete draft save. PostgreSQL quick deployment treats "create configured application" and "schedule deployment" as separate idempotent stages. If scheduling fails, the application and revision remain recoverable and the detail page offers deployment continuation; data resources are not removed and the overall operation is not presented as rolled back.

## Registry credentials

Registry credentials use filesystem-first storage under `registry-credentials/<credential-id>/`, separating metadata from immutable secret revisions and integrity-checking both. The API returns metadata such as registry, username, and revision, but never token plaintext.

Before sending a saved username or token to an OCI Bearer token service, SoloDock binds the challenge realm to the credential's Registry origin. A custom Registry must use the same scheme, exact host, and effective port; every cross-origin realm is rejected before a request is sent. Docker Hub has one built-in exception, exactly `https://auth.docker.io/token`; similar hosts, alternate paths, ports, or schemes are not trusted.

Create, rotate, and delete use an idempotency ledger and operation-owned artifacts. Delete first retains an exact tombstone. After the successful response is durable, the API, background reconciler, or startup finalizer removes it exactly. Delete fails closed while any draft, active/pending, or historical release references the credential.

Pull creates an operation-scoped Docker config only at `/run/solodock/docker-config/<deployment-id>/config.json`. This directory and credential buffers held by the process are cleaned or zeroized on every exit path. If cleanup is uncertain, the deployment records a security failure requiring attention rather than hiding it behind the original pull error.

## From tag to immutable release

1. Strictly parse and canonicalize the discovery reference, logical registry, repository, and tag.
2. Obtain the exact `repository:<repo>:pull` scope through the OCI Distribution Bearer challenge.
3. Validate response media type, header/body digest, manifest/index descriptor, and canonical host platform.
4. Record the source descriptor, index/manifest digest, and OS/architecture/variant.
5. Pull only the digest-pinned image. The Docker adapter first projects raw `ImageInspect` into an effective observation, filling missing descriptor platform fields only from the top-level fields in the same response. It then verifies canonical repository digest, config/manifest image identity, valid manifest descriptor, and top-level platform.
6. Persist the immutable release and canonical Compose and set `pending` before any Docker effect.

A tag move after resolution cannot alter the scheduled candidate. SoloDock does not currently verify Cosign/Sigstore signatures, so compromise of a trusted Registry or account remains a supply-chain risk.

## Scheduling and states

Terminal deployment states are:

- `succeeded`: the candidate passed its health gate and became active;
- `no_op`: target release and exact active/pending/actual facts are already converged, with no Docker effect;
- `failed`: deterministic failure before a Docker effect, or after proving there is no unknown side effect;
- `rolled_back`: a normal candidate failure followed by reapplication and health verification of the old active release;
- `needs_attention`: automatic recovery also failed, or safe cleanup/facts remain uncertain and require administrator judgment;
- `interrupted`: shutdown, timeout, unknown Compose/Docker effect, or scene drift where the system refuses to guess.

Nonterminal deployments are `queued` or `running`, with phases such as resolving, preparing, pulling, applying, verifying, committing, and rolling-back. A phase records ledger progress; it is not an external command that can bypass fresh facts and resume directly.

Only one mutation may run per application, and global Compose/deployment effects are also limited. The scheduling transaction persists a stable `202 Accepted` receipt, expected active/pending/actual facts, deployment, transition, and audit. `202` means only that work was durably accepted, not that deployment succeeded.

## Candidate apply and active commit

Every Compose effect path reloads and validates the following after its durable effect marker:

- current application metadata, active/pending links, and immutable release;
- HMAC, config revision, digest image, and canonical Compose;
- actual daemon data root, volume/network ownership, and bind identity;
- exact internal/bridge/label identity of the platform network when service discovery is enabled;
- every container candidate under the Compose project/service and the exact predecessor.

After the last Docker await, the fixed Compose action is called. When replacing a container, SoloDock first stops the predecessor using the grace period pinned in that predecessor's release and confirms that the old writer exited. The candidate's new grace period governs only later shutdown of the candidate. If failure occurs before a candidate is created, SoloDock first restores the stopped predecessor, then converges according to whether the result is deterministic or uncertain. Unmanaged, stale, replacement, multiple-candidate, or resource drift never reaches the runner.

The first observation after Compose is the ownership-claim boundary. One non-predecessor full container ID carrying the complete canonical project/service/application/schema/candidate-release labels proves an owned candidate, and that exact ID is immediately written to the ledger as `post_container_id`. Configured digest reference, config/manifest image identity, available manifest descriptor, platform, status, and health then determine release validity. Before the initial marker, a Docker-daemon/root actor could reproduce all canonical labels and replace a container; that actor is outside the threat model, and SoloDock does not claim causal attestation for the Compose effect.

The `ImageInspect` completion described above applies only to adapter normalization after pull. Candidate, health, no-op, failed-apply, and rollback observations read `ContainerInspect.ImageManifestDescriptor`, do not use top-level fallback, and continue to require the descriptor's own digest and canonical platform to match completely.

After `post_container_id` is durable, that exact full ID is the source of truth for compensation, health, and commit/rollback. A different full ID observed afterward is an uncertain replacement: pending and the scene remain, state becomes `interrupted` or `needs_attention`, and SoloDock must not stop or remove it. This preserves the safe compensation handle for deterministic semantic mismatches without guessing how to clean up a post-marker replacement.

Only after the candidate reaches its health policy does SoloDock atomically point `active` to the release and clear the corresponding `pending`. A replayable finalizer converges active rename, pending unlink, parent fsync, and desired-state publication. Later metadata failure cannot reverse an already visible active release.

## Failure recovery and rollback

When candidate identity, apply, or health fails deterministically and the scene is proven to belong to that candidate, SoloDock enters one compensation path. With an old active release, it stops the failed candidate using the candidate release's grace period, then reapplies and re-verifies the old release. Without an old active release, it first stops with the candidate grace period, executes an exact `rm --force`, and confirms the container is absent. Rollback repeats the old release's digest pull, resource/bind/data-root/candidate preflight, fixed Compose action, post-observation, and health gate rather than trusting historical YAML or an old container.

On an initial deployment, cleanup targets only the exact candidate already claimed and persisted as `post_container_id`; both remove result and final absence must be confirmed. An unknown effect, ownership collision, or different full ID after the marker preserves pending and the scene and becomes `interrupted` or `needs_attention`.

Terminal-state invariants are strict: `failed` proves the candidate side effect is absent, and `rolled_back` proves the old active release was reapplied and passed fresh identity and health verification. Failed or uncertain compensation can become only `needs_attention`/`interrupted`, retaining pending and exact recovery facts. It cannot masquerade as a clean terminal state. All compensation and rollback preserve volumes, bind contents, and networks and never pass volume-deletion flags.

Manual rollback also creates a deployment bound to fresh active/pending/actual facts. It restores only the release image and generated configuration, never database migrations, volumes, or bind contents.

## Registry polling

Every auto-deploy-enabled application has a generation covering its draft/source, credential revision, enabled flag, and interval. A single `PollCoordinator` maintains a bounded due heap and permits at most 2 concurrent Registry resolutions:

- a busy application is not queued and waits for a later poll;
- queued/running deployments for the same generation/target coalesce;
- an identical digest with converged active/pending/actual and configuration records a no-op;
- an unchanged digest with draft configuration changes records `config_pending_manual`;
- a failed target is durably suppressed for that generation/target;
- Registry/transient/credential errors use categorized deadlines, jitter, and bounded backoff;
- source/generation change clears old ETag and observed target so validators are never reused across Registries;
- exact-owned interrupted pending/actual may generate a new durable convergence attempt on a later poll, while unknown ownership still fails closed.

Disabling the switch prevents future polls but does not cancel a deployment already durably claimed.

## Webhook recheck

A signed webhook states only that the configured Registry tag may have changed. A valid request atomically records nonce claim, audit, and per-application wake sequence. The coalesced sequence wakes `PollCoordinator`, which still applies auto-deploy-disabled, backoff, busy, suppression, drift, and health-gate policies.

See [webhooks](webhooks.md) for the protocol, Host isolation, and WAF constraints; [application model](application-model.md) for resources; and [operations](operations.md) and [recovery](recovery.md) for manual intervention.
