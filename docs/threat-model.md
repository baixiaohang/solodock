# SoloDock threat model

> English (authoritative) · [简体中文](zh-CN/threat-model.md)

This model assumes the single-host, single-administrator boundary in [product scope](product-scope.md). See [API and streams](api-and-streams.md) for interface quotas and log redaction, and [application model](application-model.md) for container resource constraints.

## Trust boundaries

SoloDock trusts the sole administrator and host OS. Registries, images, container output, Docker/Compose output, browser input, and reverse-proxy input are untrusted. Docker socket access and `docker` group membership are effectively host root access, so Web authentication, loopback listeners, fixed actions, and systemd hardening are defense in depth, not isolation from a malicious host administrator.

Secrets are write-only. Buffers owned by this project are zeroized, and secrets do not enter API responses, SQLite, releases, audit records, argv, tracing, or errors. Logs redact against a complete, fail-closed dynamic set of secrets known to SoloDock. The kernel, allocator, Docker daemon, Registry server, and administrator business backups are outside any guarantee that plaintext never appears in memory. Backups contain secrets and require high-sensitivity protection.

A managed public/secret file leaf is `0444` on the host so an explicitly bind-mounted file can be read by a non-root container. Every host ancestor remains `0700 solodock:solodock`, and a container sees only the individual leaf named in Compose. This boundary does not isolate host root, the Docker daemon controller, or a Docker-socket holder, and does not let a managed container browse the complete state tree. Compose `read_only: true` plus immutable revisions enforce read-only behavior. Other control-plane secrets are not relaxed by this exception.

Before joining or reading a leaf, the managed-state reader validates every filename obtained from metadata. Root-relative paths accept only ordinary components and reject `.`, `..`, absolute/prefix paths, and symlink boundaries. HMAC provides content integrity; it is not deferred path sanitization. Invalid paths fail closed before target content is read.

Deployment trusts only Registry results whose digest, headers, body, and platform pass strict parsing and validation. SoloDock does not verify Cosign/Sigstore signatures. A tag race cannot alter a scheduled candidate, but a compromised Registry or image supply chain can still provide malicious content. Typed generation constrains container capabilities, mounts, and Compose. Bind allowlist and Docker data-root overlap are rechecked before every effect.

A managed container with a read-write bind is an untrusted host-filesystem writer within that source. SoloDock therefore rejects any plan where that source is a strict ancestor of another bind source. For same-application replacement it stops and verifies the exact writer before freshly resolving and revalidating binds; a conflicting writer in another application blocks the start-like action and is never stopped automatically. This closes the managed SIGTERM path-swap window, but does not claim protection from host root or an independent process that can mutate allowed paths concurrently.

Resource protections include request/body/stream/log-buffer limits, one Compose mutation, at most two Registry resolutions, poll jitter/backoff, busy coalescing, and failed-target suppression. They do not claim resistance to an actor with host root, Docker-daemon control, or a valid administrator session, and they do not provide multi-tenant isolation.

The webhook hostname is a separate public attack surface. It trusts only an HMAC from the current filesystem secret over a fixed body/path/timestamp/nonce; it does not trust proxy source IP, image facts in a payload, or forwarding headers. Nonce, wake, and audit commit atomically. Body, concurrency, and rate maps have fixed bounds. Invalid requests do not create persistent audit/replay records, preventing external storage amplification. This boundary does not replace external tunnel/WAF rate limiting.

Volumes, binds, and external networks are never deleted automatically, but their data is not rolled back with a release. External networks and members are shared daemon state. SoloDock caps member counts and inspects all member full IDs and effective DNS names within a shared deadline; truncation or partial success fails closed. An alias conflict may ignore only the exact full ID of an old container already proven by filesystem and ownership policy. Application labels, names, and short IDs do not confer replacement authority.

A slug is a constrained, globally unique, immutable human-readable namespace, not an ownership credential. The UUID application label still determines ownership. Before reuse, an owned network must match its exact name, UUID/project labels, bridge driver, and fixed option to prevent adoption of a same-named unmanaged network or wrong host interface. Users cannot provide arbitrary project, container, network, or bridge names.

Docker observation and a later Compose effect cannot form a transaction across APIs; an external root actor can change a network between them. A final fresh preflight after the durable marker narrows this window, and `NETWORK_ATTACHMENT_MISMATCH` and `NETWORK_ALIAS_MISMATCH` reveal post-effect drift. SoloDock does not claim to eliminate a concurrent actor with host root. The first container observation after Compose claims ownership using one non-predecessor full ID and all canonical candidate-release labels. A Docker-daemon/root actor that copies every canonical label and replaces the container before this marker is explicitly outside this model; SoloDock provides no causal attestation. Once `post_container_id` is persisted, its exact full ID becomes the source of truth, and any different replacement ID preserves the scene and fails closed. Administrators own application backup/restore, security updates, and schema compatibility.

See [testing and safety guardrails](testing.md) for evidence of these boundaries and [recovery](recovery.md) for manual incident handling.
