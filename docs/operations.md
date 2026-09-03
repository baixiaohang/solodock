# SoloDock operations

> English (authoritative) · [简体中文](zh-CN/operations.md)

Start with the [product scope](product-scope.md). See the [application model](application-model.md), [deployments and rollback](deployments.md), and [API and streams](api-and-streams.md) for resource, deployment-state, and system-health semantics.

## Installation and upgrade

The production target is Ubuntu 24.04 with Docker Engine and Docker Compose v2.24+. Until verified GitHub Releases are available, build the Web UI and embedded binary from source:

```bash
cd web && npm ci && npm run build && cd ..
cargo build --release --locked --features embed-ui
sudo ./packaging/install.sh --version 0.1.0 --binary target/release/solodock
```

The installer uses versioned directories and an atomic symlink. By default, it neither starts the service nor overwrites `/etc/solodock/config.toml` or `/var/lib/solodock`. Complete configuration and an offline backup before explicitly running `systemctl enable --now solodock.service`; initial installation may instead use `--enable-now`. Upgrades containing new SQLite migrations are forward-only, so switching back only the old binary is unsafe.

### One-command upgrade from a GitHub build

The installer also installs `/usr/local/bin/solodock-update`. First confirm that GitHub CLI supports `gh attestation verify`, then log in once with the day-to-day administrator account. Its token needs only read access to the repository, Actions artifacts, and artifact attestations; never place the token in scripts, configuration, or command arguments:

```bash
gh auth login --hostname github.com
solodock-update
```

This updater is currently a development channel backed by expiring `main` workflow artifacts, not a stable release channel. It first reuses existing or passwordless `sudo` authorization and prompts once in an interactive terminal only if needed. Without a TTY or configured noninteractive `sudo`, it fails before modifying the service.

The updater selects the latest successful `push` CI run on the target branch and downloads the run's rebuilt and verified `solodock-embedded-package`. Before backup, service stop, or installation, it uses GitHub CLI to verify the GitHub artifact attestation for `SHA256SUMS`: the signing workflow must be the target repository's `.github/workflows/ci.yml`, source ref and commit must exactly match the selected run, and proofs from self-hosted runners are rejected. It then verifies every SHA-256 in the package and requires `SOURCE_SHA` to exactly match the workflow run commit. Missing or expired artifacts/attestations, signing-identity or source-commit mismatches, and checksum failures all fail closed. If the new binary matches the current binary, the service is not stopped. Otherwise, the updater stops SoloDock, creates an offline control-plane backup under `/var/backups/solodock/`, installs to a `main-<commit SHA>` version directory, starts the service, and checks loopback `/healthz` and `/favicon.svg`. Temporary artifacts are cleaned on every exit path. Application containers, volumes, and bind data are outside its scope.

Run the updater only as an explicit administrator maintenance operation, not from an unattended timer. After a new binary has been started, health failure does not automatically switch back because SQLite migrations are forward-only. Retain the backup and scene and follow this page and [recovery](recovery.md). Use `solodock-update --help` for nondefault repository, branch, workflow selector, backup directory, or loopback port. The workflow selector must still identify trusted `.github/workflows/ci.yml`; it cannot select an arbitrary workflow as a release source.

## Security prerequisites

The service listens only on loopback and `public_origin` must use HTTPS. An external tunnel or reverse proxy, access control, and TLS are deployment prerequisites that SoloDock does not configure. The `solodock` user belongs to the `docker` group, which is effectively host root access; restrict host administrators, configuration files, and the Web login surface accordingly.

When enabling webhooks, configure a distinct authority in `webhook_public_origin` and allow only the exact POST path at the external WAF. See [webhooks](webhooks.md) for signature, timestamp/nonce, retry, and 202 semantics.

Complete one-time bootstrap from `/run/solodock/bootstrap.token` after the first start. Routine checks:

```bash
systemctl status solodock.service
journalctl -u solodock.service --since today
curl --fail http://127.0.0.1:8080/healthz
```

Authenticated `/api/v1/system/health` reports Docker, recovery, projection, deployment, poll coordinator, disk, and credential states separately. For `interrupted`, `needs_attention`, or an ownership collision, inspect deployment details and the exact `docker inspect` facts first. Do not prune, delete broadly, or retry speculatively.

Administrators must create every referenced external network before use. SoloDock never changes its driver, IPAM, labels, or lifecycle, and upgrades, deployments, and deletion never remove it. The default `solodock-services` network for new services is not a user external network: SoloDock creates it on first use as an internal bridge and strictly validates `sd-services` and platform labels. On `PLATFORM_NETWORK_IDENTITY_CONFLICT`, identify the same-named resource's owner rather than letting SoloDock adopt or delete it. Applications can use the slug and container port for internal access.

Use the bridge name shown on application details for an owned network. Old applications may use `sd-<slug>`, while new applications use a UUID-derived token. Before configuring UFW/nftables, verify the identity in the UI, with `ip link show`, and with `docker network inspect solodock-<slug>-default`; do not guess a bridge from the slug. The internal platform network uses `sd-services` and does not replace an application's owned network for outbound access.

`NETWORK_BRIDGE_IDENTITY_CONFLICT` means an existing owned network's driver or bridge option differs from the canonical identity. SoloDock neither deletes nor adopts that network. Stop affected containers, verify ownership, let an administrator resolve the conflicting resource, and redeploy.

Application shutdown grace defaults to `10` seconds and can be set from `1–600` seconds during registration or configuration. It is the maximum before SIGKILL, not a fixed delay. Applications that flush data, drain queues, or perform final synchronization should increase it to match their shutdown contract. Deploy/recreate predecessor stop, manual stop/restart, explicit remove, and failed rollback all use the value pinned in the release being stopped.

## Automatic deployment and credentials

An administrator must explicitly confirm automatic deployment. Disabling it prevents future polls but does not cancel a deployment already durably claimed. `config_pending_manual` means the digest is unchanged but the draft configuration differs and requires Deploy. `suppressed_failed_target` means that target already failed or rolled back; inspect health and data compatibility, then use a new digest/config or an explicit manual deployment to clear it. Rotating a Registry credential changes the generation and returns it to jittered polling.

For disk alerts, expand capacity or remove only confirmed-unused content outside SoloDock state. Never delete state revisions/ledgers, Docker volumes, or bind sources. `MemoryHigh=256M` is soft pressure; there is no `MemoryMax`.

The system-health "host memory available" value is Linux `/proc/meminfo` `MemAvailable`, distinct from an application container's memory usage. The same parser drives the 128 MiB image-pull memory gate. Missing, malformed, or invalid values report unknown and degrade health instead of pretending to be zero.

Manage bind allow roots in **System settings → Storage access**. SoloDock validates existing absolute directories and permits applications to reference them; it has no directory browser and runs no `mkdir/chown/chmod/rm`. TOML values are imported once during upgrade, then the Web UI is authoritative. If removal returns `BIND_ROOT_IN_USE`, remove the bind from every listed draft/active/pending configuration and complete a safe data migration first.

PostgreSQL quick deployment defaults to major 18 and an owned volume at `/var/lib/postgresql`; major 17 targets `/var/lib/postgresql/data`. Changing major does not modify an existing volume target or migrate data. Follow PostgreSQL's own backup, migration, and acceptance process. The database publishes no host port by default; new services use `<postgres-slug>:5432`.

The global display timezone is selected in **System settings** from the backend IANA tzdb list and stored in the singleton SQLite settings record, defaulting to `UTC`. The mutation uses revision, idempotency key, Origin, session, and CSRF. Saving redraws Web timestamps without restart. It neither injects `TZ` into managed containers nor changes UTC values in the database, API, SSE, cursors, expiry checks, or downloaded logs. If the browser cannot render a saved zone, the UI warns and falls back to UTC.

## Backup

Stop the service before backup:

```bash
sudo systemctl stop solodock.service
sudo ./packaging/solodock-backup --output /secure/new/solodock-control-plane.tar
```

The archive contains application, Registry credential, and webhook secrets. Restrict it as highly sensitive data and encrypt it independently. It retains the immutable revision's network mode and aliases but excludes business volumes, bind data, Docker images/containers, and networks. Recreate required external networks separately before recovery, and maintain an independent, tested restore-capable backup for each workload.

Before restoring an archive or resolving a degraded/interrupted state, follow the fail-closed process in [recovery](recovery.md). See the [threat model](threat-model.md) for security prerequisites.
