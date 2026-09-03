# SoloDock recovery

> English (authoritative) · [简体中文](zh-CN/recovery.md)

This document covers recovery of the control plane and managed state only. See [architecture](architecture.md) for sources of truth and publication boundaries, and [deployments and rollback](deployments.md) for active/pending/actual facts and terminal deployment states.

## Offline recovery

Recovery is supported only while the service is stopped. Verify the SHA-256 file beside the archive, extract into a private temporary directory, and reject absolute paths, `..`, hard links, special files, and unexpected top-level entries. The only allowed symlinks are SoloDock-owned canonical `apps/<app UUID>/{active,pending} -> releases/<release UUID>` links. In private staging, the restore helper normalizes legacy `0400`/`0600` modes on canonical managed leaves to `0444`, then invokes the same package binary to validate owner/mode, link boundaries, HMACs, config revisions, and canonical Compose. It does not repair `0644`, owner drift, symlinks, or special files. Never overwrite live state. Atomically rename the current `/var/lib/solodock` to a recoverable backup before switching in the complete validated state/config. Keep directories at `0700`, ordinary control-plane files at `0600`, and direct `files/{public,secret}` leaves at `0444`, then start the service and inspect the journal, `/healthz`, and authenticated system health.

Binary, configuration, and state must come from one compatible backup set. SQLite migrations are forward-only; reverting only the binary after migration is unsafe.

## Failure categories

- **Lost SQLite:** filesystem facts rebuild application/release query projections, but administrator credentials, sessions, audit, and deployment history cannot be recreated; bootstrap is required.
- **Degraded filesystem:** stop mutations and retain old redactor patterns. Verify owner/mode, missing revisions, and symlink boundaries, then restart. Startup recovery normalizes only known legacy `0400`/`0600` canonical managed leaves to `0444`; runtime projection scans are strictly read-only. Repair other drift only offline after backup. Do not recursively set every state file to `0600` or manually edit HMAC-protected releases.
- **Docker drift:** inspect exact project/service/full container IDs for unmanaged, stale, or multiple candidates. Read expected networks from the immutable active/pending config revision corresponding to the actual container's release ID, never the current draft. For `NETWORK_ATTACHMENT_MISMATCH`, compare network-name sets; for `NETWORK_ALIAS_MISMATCH`, ensure expected aliases are a subset of effective DNS names. Back up business data and choose an explicit repair. Never use wildcard cleanup, `docker compose down -v`, or `docker volume rm`.
- **Bridge drift:** use the versioned bridge projected on application details instead of guessing a new application's token from its slug. Inspect network ownership, driver, and `Options.com.docker.network.bridge.name`. SoloDock fails closed on mismatches and never deletes them automatically.
- **Platform-network drift:** `solodock-services` must be an internal `bridge` using host bridge `sd-services` and exact platform labels. SoloDock does not adopt a same-named unmanaged or drifted resource. Stop affected releases and have an administrator confirm the resource source before recovery.
- **Deployment `interrupted`/`needs_attention`:** use exact pending/active/actual facts in details to retry or manually roll back. Never speculatively delete an unknown effect.
- **Credential tombstone:** startup/background finalizers clean only when the ledger proves exact success; unknown markers fail closed.
- **Poll suppression:** after repairing the application/health, use manual Deploy or publish a new digest/config. Do not edit SQLite directly.
- **Degraded webhook:** keep the endpoint fail closed. Repair owner/mode/HMAC of `webhook.toml` and its immutable secret revision, then restart. Do not edit secret metadata manually. Losing SQLite loses nonce history and wake operational state but does not fabricate webhook audit.

After recovery, recreate every external network referenced by immutable active/pending revisions but missing from the daemon. The first deployment recreates the platform network only after exact identity preflight. Then verify each active digest, container full ID, health, port, volume/bind/network canary, platform slug DNS, and external alias. SoloDock release rollback does not roll back a database or persistent data.

Legacy naming/config/release schemas remain readable and rollback-capable. Old applications retain their bridge and do not automatically join the platform network. Old Compose schemas first verify the release HMAC and exact file hashes covered by its signature, then validate the canonical document for that schema. Serializer quoting changes do not mark a valid old release as corrupt or relax content/structure validation. Do not inject new fields into historical artifacts manually; readers ignore control values outside the old signed domain. Restore SQLite global settings from the same point as filesystem state, or bind-root revisions may disagree with application references and make recovery fail closed.

See [operations](operations.md) for routine installation, backup, and health checks, and [application model](application-model.md) for resource-retention and deletion semantics.
