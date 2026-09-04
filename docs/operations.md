# SoloDock operations

> English (authoritative) · [简体中文](zh-CN/operations.md)

Start with the [product scope](product-scope.md). See the [application model](application-model.md), [deployments and rollback](deployments.md), and [API and streams](api-and-streams.md) for resource, deployment-state, and system-health semantics.

## Installation and upgrade

The production target is Ubuntu 24.04 x86_64 with Docker Engine, the `docker` group/socket, systemd, and Docker Compose v2.24+. Stable GitHub Releases provide a long-lived package and require GitHub CLI authentication plus the `gh attestation verify` capability; they do not require Rust, Node.js, npm, or Git on the host. Distribution packages can lag behind GitHub CLI capabilities. Install or upgrade `gh` with GitHub's [official Linux instructions](https://github.com/cli/cli/blob/trunk/docs/install_linux.md), and treat this capability check—not a permanently documented version number—as authoritative:

```bash
gh attestation verify --help
gh auth login --hostname github.com
gh auth status
```

The following Bash session selects GitHub's actual Latest Release, verifies that it is published and stable, requires its canonical `vMAJOR.MINOR.PATCH` tag, resolves the tag to an immutable commit, downloads the three exact assets, verifies the `SHA256SUMS` provenance and source identity, checks both checksum layers, and installs the packaged binary:

```bash
set -euo pipefail
repo=baixiaohang/solodock
release_data=$(gh release view --repo "$repo" --json tagName,isDraft,isPrerelease \
  --jq '[.tagName, .isDraft, .isPrerelease] | @tsv')
IFS=$'\t' read -r tag is_draft is_prerelease <<<"$release_data"
[[ $is_draft == false && $is_prerelease == false ]]
[[ $tag =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]
source_sha=$(gh api "repos/$repo/commits/$tag" --jq .sha)
[[ $source_sha =~ ^[0-9a-f]{40}$ ]]
asset="solodock-${tag}-ubuntu-24.04-x86_64.tar.gz"
download_dir=$(mktemp -d)
trap 'rm -rf -- "$download_dir"' EXIT
gh release download "$tag" --repo "$repo" --dir "$download_dir" \
  --pattern "$asset" --pattern SHA256SUMS --pattern SOURCE_SHA
gh attestation verify "$download_dir/SHA256SUMS" \
  --repo "$repo" \
  --signer-workflow "$repo/.github/workflows/release.yml" \
  --source-ref "refs/tags/$tag" \
  --source-digest "$source_sha" \
  --deny-self-hosted-runners
(cd "$download_dir" && sha256sum -c SHA256SUMS)
[[ $(<"$download_dir/SOURCE_SHA") == "$source_sha" ]]
tar -xzf "$download_dir/$asset" -C "$download_dir"
package="$download_dir/solodock-package"
(cd "$package" && sha256sum -c SHA256SUMS)
[[ $(<"$package/SOURCE_SHA") == "$source_sha" ]]
[[ $(<"$package/VERSION") == "${tag#v}" ]]
"$package/verify-package.sh" "$package"
sudo "$package/install.sh" --version "${tag#v}"
```

The package's checksummed `INSTALL_MANIFEST` binds its `stable` channel, version, source commit, and full package-content identity. The installer verifies that manifest and prepares an immutable, identity-qualified generation containing the binary, manifest, updater, package verifier, backup/restore helpers, and systemd unit. It snapshots every existing public entry, switches helpers and the unit first, and switches `/usr/local/bin/solodock` last as the installation-identity commit marker. Any failure before that transaction commits restores every entry and removes the incomplete generation, so the visible binary, helpers, unit, and manifest remain from one package. `/usr/local/bin/solodock-restore` resolves its generation-bound sibling `solodock` binary as the validator by default. Backup and restore outputs must be placed in a canonical directory owned by the invoking administrator and not writable by group or other; each ancestor must be administrator/root-owned and any writable ancestor must have sticky-directory protection. The helpers anchor all temporary and publication paths to the checked directory identity, recheck its path/device/inode before and after publication, use exclusive unpredictable temporary names, and never replace an existing archive, checksum, or restore target. The installer neither starts the service nor overwrites `/etc/solodock/config.toml` or `/var/lib/solodock` unless initial installation explicitly uses `--enable-now`. Complete configuration and an offline backup before starting. Upgrades containing new SQLite migrations are forward-only, so switching back only the old binary is unsafe.

The official packaged profile fixes the config file at `/etc/solodock/config.toml`, state at `/var/lib/solodock`, and runtime files at `/run/solodock`. `listen_address` remains configurable but must be an explicit loopback socket; public origins remain configurable canonical HTTPS origins. The systemd unit selects this profile with exact `SOLODOCK_PACKAGED_LAYOUT=1`. The runtime rejects an invalid marker or path drift before creating managed directories or opening SQLite. The installer, updater, backup, and restore invoke the package generation's side-effect-free Rust config inspector before their own mutation boundary; they never parse an arbitrary TOML path in Bash.

An existing source deployment with custom paths must be migrated before using the package updater. Stop its old service, independently back up the actual configured state and config, move the complete state to `/var/lib/solodock` with the exact `solodock:solodock` ownership and documented modes, install the config at `/etc/solodock/config.toml` with `state_directory = "/var/lib/solodock"` and `runtime_directory = "/run/solodock"`, and verify the old binary on that fixed layout before invoking `solodock-update`. Keep the independent pre-migration backup until the new installation is accepted. The new updater's preflight refusal occurs before it stops the service and therefore is not a migration backup.

### Verified stable and main upgrades

The installer also installs `/usr/local/bin/solodock-update`. Log in once with the day-to-day administrator account; its token needs only the read access required for the repository, Release or Actions artifact, and artifact attestation. Never place the token in scripts, configuration, or command arguments:

```bash
gh attestation verify --help
gh auth login --hostname github.com
gh auth status
solodock-update
```

With no `--channel`, the updater reads the current version-bound `INSTALL_MANIFEST`: a Release installation continues on `stable`, and a CI installation continues on `main`. Passing `--channel stable|main` explicitly switches tracks once; the successfully installed package records the new channel for later no-argument runs. A legacy installation without a manifest is inferred only from an exact managed `main-<12-hex>` or canonical SemVer directory; any other form fails closed and requires an explicit channel. The `--branch` and `--workflow` selectors are main-only, and invalid combinations are rejected before authentication, download, `sudo`, or service changes.

The `stable` channel reads GitHub's actual Latest Release and requires it to be published, non-draft, and non-prerelease. The release workflow leaves Latest selection to GitHub's version-aware default instead of forcing every newly created older-line release to become Latest. If GitHub ever reports a stable version lower than the installed stable manifest, the updater refuses the downgrade before download or mutation. The updater reuses existing or passwordless `sudo` authorization and prompts once in an interactive terminal only if needed. Without a TTY or configured noninteractive `sudo`, it fails before modifying the service.

Stable discovery downloads the exact versioned Ubuntu archive plus `SHA256SUMS` and `SOURCE_SHA`. It resolves the Release tag to a commit, requires canonical tag/package version agreement, and verifies the checksum attestation against `.github/workflows/release.yml`, the exact tag ref and commit, and a GitHub-hosted runner. Main discovery instead selects the latest successful `push` CI run and retains the existing `.github/workflows/ci.yml`, branch, commit, and GitHub-hosted-runner attestation policy. Missing or expired artifacts/attestations, a moving or invalid Release identity, and any source, version, or checksum mismatch fail closed.

After discovery, both channels use the same package validation and apply path. Before stopping the service, taking its offline backup, or invoking the installer, the updater runs the fully verified downloaded binary's config inspector against `/etc/solodock/config.toml`. It rejects a custom layout or malformed output without using fallback values, and derives the exact IPv4 or bracketed IPv6 loopback `/healthz` URL from that record; there is no `--health-url` override. Currentness requires the complete package identity in the installed manifest, the selected immutable generation, binary, updater, package verifier, backup/restore helpers, their managed symlinks, and systemd unit all to match the verified package. If only package identity or helpers changed while the binary digest is unchanged—including a main-to-stable transition—the updater transactionally publishes a new generation and verifies the running service without stopping or invoking the binary. If the binary changed, the updater stops SoloDock, creates an offline control-plane backup under `/var/backups/solodock/`, transactionally publishes a stable SemVer or `main-<commit SHA prefix>` generation, starts the service, and checks loopback `/healthz` and `/favicon.svg`. A pre-invocation install failure restores and verifies the complete old package generation before the updater restarts the old service. If any rollback operation or verification fails, the installer returns a distinct incomplete-rollback status, preserves the transaction scene, and the updater leaves or puts the service in the stopped state with manual recovery instructions; it never starts whichever binary remains linked. After the new binary is invoked, the forward-only rule applies. Temporary downloads are cleaned on every exit path. Application containers, volumes, and bind data remain outside its scope.

After authentication, the sidebar reads `/api/v1/system/installation` and displays this same installation identity. Stable installations show the SemVer, channel, and short source commit; main installations show `main` and the source commit. Expanding the entry exposes the full source SHA and package identity for issue reports. The endpoint reads the fixed managed symlink and manifest on every request, so a package-only channel change is visible after the next page load without restarting SoloDock. Local source runs report `development`; a missing, damaged, or noncanonical managed manifest reports `unknown` without degrading other control-plane functions. The public `/healthz` and unauthenticated sign-in page do not expose this fingerprint.

Run the updater only as an explicit administrator maintenance operation, not from an unattended timer. After a new binary has been started, health failure does not automatically switch back because SQLite migrations are forward-only. Retain the backup and scene and follow this page and [recovery](recovery.md). Use `solodock-update --help` for channel, repository, main selector, and backup-directory options; the loopback probe address always comes from the installed config.

GitHub **Release assets** are the long-lived stable distribution. **Actions artifacts** are 30-day development outputs used by the main channel. **GitHub Packages** is a separate registry for containers and language packages; SoloDock publishes neither there and does not depend on it.

## Security prerequisites

The service listens only on loopback and `public_origin` must use HTTPS. An external tunnel or reverse proxy, access control, and TLS are deployment prerequisites that SoloDock does not configure. Configure the proxy to preserve the exact external `Host` from `public_origin`; rewriting it to the loopback upstream authority causes management requests to fail closed with `404`. `Forwarded`, `X-Forwarded-Host`, `X-Original-Host`, and similar headers are intentionally ignored for routing authority. The `solodock` user belongs to the `docker` group, which is effectively host root access; restrict host administrators, configuration files, and the Web login surface accordingly.

The packaged unit has only `After=`/`Wants=` ordering for Docker. An absent socket or stopped daemon is a supported degraded state: SoloDock remains running so `/healthz`, authentication, filesystem catalog, and recovery information stay available, while Docker-dependent operations retain their specific degraded failures. If `/var/run/docker.sock` exists, the installer still requires a Unix socket owned by the configured `docker` group; a regular file, symlink, or wrong-group socket is never treated as degraded. Initial import of non-empty legacy TOML bind roots may still require an observable Docker data root to prove non-overlap and remains fail closed.

When enabling webhooks, configure a distinct authority in `webhook_public_origin` and allow only exact `POST /hooks/v1/apps/<canonical-lowercase-UUID>/registry` at the external WAF. The webhook authority rejects UI, management API, GET, and noncanonical paths; the management authority rejects webhook paths. See [webhooks](webhooks.md) for signature, timestamp/nonce, retry, and 202 semantics.

Complete one-time bootstrap from `/run/solodock/bootstrap.token` after the first start. Routine checks:

```bash
systemctl status solodock.service
journalctl -u solodock.service --since today
curl --fail http://127.0.0.1:8080/healthz
```

The loopback listen authority permanently exposes only exact `GET /healthz` and `GET /favicon.svg` when it differs from `public_origin`, so installed updaters can probe a strict-Host binary. It does not expose the sign-in page, other assets, management API, SSE, or webhook routes. For direct diagnostics, keep the configured loopback authority in the request URL; do not substitute forwarding headers.

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

The Web UI language can be changed between English and Simplified Chinese on the bootstrap/sign-in screen and in the authenticated header. SoloDock stores an explicit choice only in browser `localStorage` under the versioned, non-sensitive key `solodock.ui.locale.v1`; it never sends the locale to the API, session, audit log, URL, SQLite, or server settings. Without a valid stored value, a first preferred browser language of `zh` or `zh-*` selects `zh-CN`; every other value selects English. Unavailable storage and invalid values fail safely without blocking the UI. Language changes immediately update visible text, localized timestamps, accessibility labels, and the document `lang` attribute.

## Backup

Stop the service before backup:

```bash
sudo systemctl stop solodock.service
sudo /usr/local/bin/solodock-backup --output /secure/new/solodock-control-plane.tar
```

The archive contains application, Registry credential, and webhook secrets. Restrict it as highly sensitive data and encrypt it independently. It retains the immutable revision's network mode and aliases but excludes business volumes, bind data, Docker images/containers, and networks. Recreate required external networks separately before recovery, and maintain an independent, tested restore-capable backup for each workload.

The backup helper resolves the `solodock` binary from its own immutable package generation and validates the fixed packaged layout before creating an archive or temporary output. Restore applies the same check to the extracted config before changing ownership/modes or publishing a target. A custom-path config is refused rather than partially archived or silently followed.

Before restoring an archive or resolving a degraded/interrupted state, follow the fail-closed process in [recovery](recovery.md). See the [threat model](threat-model.md) for security prerequisites.
