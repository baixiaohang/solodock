# SoloDock application model

> English (authoritative) · [简体中文](zh-CN/application-model.md)

SoloDock manages only structured, single-service applications. The administrator fills supported fields, and SoloDock generates and validates canonical Compose. There is no general Compose import or raw YAML editor.

## Applications, drafts, and projects

An application has an immutable UUID and immutable slug. The UUID is the authoritative identity in API paths, filesystem directories, and ownership labels. A new slug must be globally unique, contain 1–20 lowercase ASCII letters, digits, or hyphens, and begin and end with a letter or digit. A slug cannot change after creation; `display_name` remains editable.

A single versioned helper controls resource naming. The Compose project is `solodock-<slug>`, the default container is normally `solodock-<slug>-app-1`, an owned network is `solodock-<slug>-default`, and an owned volume is `solodock-<slug>.<logical-name>`. Legacy naming-v1 applications retain their `sd-<slug>` bridge. New naming-v2 applications use a stable UUID-derived `sd-<12-char-token>` bridge, allowing 20-character slugs without changing historical UFW/nftables identities. Users cannot set `container_name` or arbitrary Docker resource names.

Blank creation writes only application metadata; it does not fabricate an empty config revision or create Docker resources. The detail state is then `UNCONFIGURED`; draft, image, credential, active, and pending are empty, and desired state is stopped. The first configuration save reuses the normal revision mutation and accepts `expected_revision = null` only when no draft exists. Deploy, start, poll, and webhook operations deterministically return `APP_UNCONFIGURED` for an unconfigured application.

`app.toml` points to the current draft config revision and records desired state, Registry polling configuration, and the last filesystem mutation. Every edit fully publishes a new `config-revisions/<revision-id>/` before atomically replacing metadata as the commit point. A published release pins its own config revision, so later draft edits cannot alter the files mounted by an active or pending container.

The draft also contains `stop_grace_period_seconds`, with a range of `1–600` and a default of `10`. A release pins this value with its config revision and emits Compose `services.app.stop_grace_period`. It is the maximum time allowed for graceful shutdown after SIGTERM, not a fixed delay; lifecycle or deployment proceeds as soon as the service exits. Legacy config revisions and releases without this field are interpreted as `10` seconds and retain their original canonical hash/HMAC.

## Images and credentials

A draft stores a tagged discovery image reference. The tag is used only for Registry discovery. Releases and Compose use:

```text
<canonical-registry>/<repository>@sha256:<manifest-digest>
```

For a multi-platform image, SoloDock records the source descriptor, optional index digest, selected OS/architecture/variant, manifest digest, and image config digest. Historical v2 releases still serialize the config digest under `local_image_id` to preserve HMAC and stored-file compatibility. Docker Engine may represent an image/container ID as either the config digest or selected manifest digest; SoloDock matches both through one identity object. Legacy/classic daemons that do not return a manifest descriptor fall back to that digest set. With Docker 29's containerd image store, raw `ImageInspect.Descriptor` may include a digest but omit its platform. The Docker adapter fills only missing OS/architecture/variant fields from the top-level fields in the same `ImageInspect` response, creating an effective observation without overwriting descriptor values. Once an effective descriptor exists, its digest and canonical platform must match completely; missing, malformed, or conflicting values fail closed. `ContainerInspect.ImageManifestDescriptor` has no such fallback and is always validated from its own fields. An application may reference one write-only Registry credential whose logical registry matches exactly. See [deployments and rollback](deployments.md) for credential lifecycle.

## Environment variables

Environment variables have one canonical data model. Public variables can switch losslessly between a row editor and bulk `KEY=VALUE` text. Bulk mode splits on the first `=`, ignores blank lines, and reports line numbers for missing separators, invalid keys, and duplicate keys. Secrets remain in a separate write-only row editor; they never enter bulk text or use placeholders that could be submitted accidentally.

- Public values can be read and edited.
- A saved secret displays only a "saved" marker with a blank value. Leaving blank, entering a new value, or deleting the row maps to `keep`, `replace`, or `delete`.
- The API, UI, SQLite, releases, Compose, audit, errors, and tracing never echo secrets.
- Moving a key between public and secret classifications requires explicit deletion from the old class and submission to the new class.
- Duplicate keys, invalid names, interpolation, and command substitution syntax are rejected.

Generated Compose contains no secret plaintext; it references permission-constrained managed files.

## Managed files

A managed text file has a logical name, container target path, sensitive flag, and read-only property. Public contents can be read. Secret contents use the same write-only operation model as secret environment variables. Config revisions enforce per-file and aggregate quotas and store public and secret contents in separate permission boundaries.

On the host, the state root, application directory, config revision, and `files/{public,secret}` directories remain `0700 solodock:solodock`. Only a direct leaf mounted into a container at `files/public/<logical-name>` or `files/secret/<logical-name>` is exactly `0444 solodock:solodock`. This lets common non-root container UIDs/GIDs read a file explicitly mounted into their container while ordinary host users cannot traverse or enumerate the private state tree. Environment secrets, Registry/webhook credentials, SQLite, release metadata, and other control-plane files do not use this exception and remain private.

Compose still mounts every managed file with `read_only: true`, so the container cannot write to the host inode. Persistent writable content must use a volume or an explicitly confirmed read-write bind, not managed files that bypass secret, quota, or immutable-release semantics. Publication writes into a private temporary revision, applies the final mode, and fsyncs before the revision becomes atomically visible. Before deployment, the strict loader rejects mode, owner, file-type, or symlink drift and returns the configuration- or release-invalid error appropriate to that deployment phase.

## Ports, volumes, binds, and networks

### Ports

Published ports must explicitly use a loopback host IP and specify TCP or UDP. SoloDock rejects non-loopback application publishing.

### Named volumes

- An owned volume maps an application logical name to an internal Compose resource key.
- An external volume must already exist; SoloDock does not change its ownership.
- Lifecycle, deploy, rollback, unregister, remove, and deletion never pass volume-deletion flags.

### Bind mounts

SQLite global settings maintain the allowed bind roots. The default list is empty, so binds are disabled by default. During upgrade, TOML `allowed_bind_roots` is imported only once; the UI and SQLite are authoritative afterward. Removing a root still referenced by a draft, active, or pending revision returns a conflict. When binds are enabled:

- a root must be an existing, absolute, symlink-free private path;
- a source must be a strict descendant, never the root itself;
- neither root nor source may overlap state, runtime, the Docker socket, sensitive system directories, or the daemon's actual data root;
- validate, preview, and every Docker effect recheck the canonical path, symlinks, device/inode, and data root;
- binds are read-only by default; each read-write bind must explicitly acknowledge in its own row that release rollback cannot revert it, and changing to read-only and back resets that acknowledgment;
- a read-write source must not be a strict ancestor of any other bind source in the same configuration or in another running managed application; identical sources and sibling sources are not ancestor conflicts;
- SoloDock never creates, `chown`s, `chmod`s, backs up, or deletes a source.

Saving a draft performs an early ancestor check. Every start-like effect repeats the check against a fresh inventory of live managed applications. An application replacement or restart first stops its exact owned writer container, confirms that it exited, and then revalidates the target paths before starting anything. A cross-application conflict blocks only start, deploy, restart, rollback, or recovery of the conflicting application with `BIND_SOURCE_ANCESTOR_CONFLICT`; SoloDock never stops the other application automatically, and read, stop, and corrective edit operations remain available.

### Networks

New applications attach by default to both the application's owned default network and the platform's internal service-discovery network. The owned network bridge identity follows the application's naming schema. The platform network is the internal `solodock-services` network with host bridge `sd-services` and exact platform ownership labels. A single manager inspects or creates it on first use. A same-named resource with mismatched driver, internal flag, bridge, or labels fails closed and is never adopted. The application's DNS alias on the platform network is its slug, enabling `<slug>:<container-port>` communication. The platform network is not deleted with any single application.

In config schemas 1/2 and release schemas 3/4, the only valid `service_discovery_enabled` value is `false`; injecting the field into an old signed domain cannot expand network access. Existing applications join the platform network only after explicitly saving a new revision/release. Configuration may also attach up to 8 existing external networks or disable owned/platform networking in advanced settings. A configuration with all three categories disabled is rejected.

Each external attachment can assign up to 8 stable aliases to the sole `app` service. An alias is a lowercase DNS label unique within the attachment. Networks and aliases enter config SHA, release integrity, and Compose in canonical order. Legacy releases without aliases keep Compose network short syntax; default booleans and empty aliases do not enter legacy canonical serialization.

Administrators must create external networks in advance. Before an effect, a fresh Docker snapshot validates network existence, member full IDs, and effective DNS names. An alias held by an unrelated container, incomplete member observation, or intervening change fails closed. Only the exact full ID of a verified predecessor container may be ignored. SoloDock never creates, modifies, or removes an external network. Unregister and container removal also preserve owned networks.

Container target paths for volumes, binds, and managed files must not conflict. Preview shows canonical source and target, read-only state, and owned/external status.

## Health policies

An application selects one health policy:

- `healthy`: require Docker health to become `healthy`, using either the image healthcheck or a structured HTTP healthcheck;
- `running`: require the same container to remain running for a stability window, default 15 seconds;
- `completed`: require a one-shot workload to exit with code 0;
- `disabled`: explicitly acknowledge reduced safety and prove only a bounded running condition.

The health policy governs deployment commit and rollback verification. Changing a draft policy does not retroactively alter existing releases.

Rust domain constants define healthcheck numeric limits and defaults. `GET /api/v1/settings` exposes them to the Web UI under `configuration_limits.health`; the frontend does not maintain a second set of limits. Configuration editing fails closed if the capability is unavailable, preventing the browser from accepting values the backend must reject.

## Lifecycle and deletion

Start, stop, restart, and remove act only on objects that pass complete project/service/application/release/schema/full-container-ID ownership validation. Stop and restart use the operated release's pinned grace period. Remove explicitly stops with that same grace period before deleting the stopped container. Unmanaged, stale, multiple, or malformed candidates always fail closed.

Deletion is a two-phase protocol:

1. Preview builds canonical facts from fresh filesystem data, active/pending/draft configurations, and exact Docker observations.
2. DELETE submits the confirmation token, slug, and disposition and recomputes the facts hash before token consumption and before the filesystem tombstone.

Preview merges files, volumes, binds, and networks from active, pending, and draft, distinguishing present resources from configured-only resources. Network facts include owned/external kind, owned bridge name, sorted aliases, and configuration scope; an external-only revision does not invent an owned default network. A configured or degraded webhook conservatively warns that its write-only secret is permanently deleted with the application tombstone.

Deletion unregisters by default. Explicit removal still removes only the exact owned container bound to the token; all volumes, bind contents, and networks remain. A rollback-capable barrier coordinates deletion with stream producers, and new streams become permanently unavailable only after the filesystem tombstone commits.

See [deployments and rollback](deployments.md) for active/pending and rollback semantics, and [recovery](recovery.md) for file-permission and link requirements.
