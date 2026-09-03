# SoloDock product scope

> English (authoritative) · [简体中文](zh-CN/product-scope.md)

SoloDock is a lightweight, single-host deployment console for personal Docker workloads. A single Rust process serves the management API and embedded Web UI, resolves mutable image tags to immutable manifest digests, and runs a constrained one-container release flow with health gates, failure recovery, and manual rollback.

```text
one SoloDock application = one Compose project = one app service/container = one prebuilt image
```

SoloDock is neither a general Docker management panel nor a complete PaaS. Its deliberately narrow model lets a Docker-root control plane operate through auditable fields, fixed actions, and exact ownership on a resource-constrained personal host.

## Target environment

- One Ubuntu 24.04 host.
- One administrator, with no multi-tenancy or RBAC.
- Docker Engine and Docker Compose v2.24+.
- Services listen only on loopback. An external tunnel or reverse proxy provides public access, access control, and TLS.
- A typical ceiling of 2 vCPU and 4 GiB, with fixed control-plane deployment concurrency limits.

Access to the Docker socket or membership in the `docker` group is effectively host root access. A dedicated system user, loopback listeners, and systemd hardening are defense in depth, not low-privilege isolation.

## Current capabilities

SoloDock currently supports:

- one-time local bootstrap, single-administrator sessions, CSRF protection, exact Origin validation, and authentication audit;
- registration of multiple managed single-service applications and generated canonical Compose;
- public and write-only secret environment variables and managed text files;
- loopback ports, owned/external named volumes, constrained bind mounts, and owned/external networks;
- start, stop, restart, validate, deploy, rollback, unregister, and exact container removal;
- filesystem-first immutable config revisions and releases, plus `active` and `pending` pointers;
- public/private OCI Registry resolution, digest-only pulls, multi-platform image selection, and deployment history;
- health gates, deterministic failure recovery, unknown-effect interruption protection, and manual convergence;
- Registry polling, automatic digest deployment, no-op/coalescing/backoff, and failed-target suppression;
- an optional signed Registry recheck webhook;
- bounded events, logs, and stats SSE with fail-closed secret redaction;
- an embedded production UI, systemd installation, offline control-plane backup/restore, and resource acceptance tests.

## Explicit non-goals

SoloDock does not provide:

- source repository cloning, Dockerfile/buildpack builds, or `docker compose build`;
- arbitrary Compose import, raw YAML editing, multiple services, replicas, or adoption of existing projects;
- reverse proxy, tunnel, DNS, TLS, WAF, or host firewall configuration;
- Docker Swarm, Kubernetes, multiple hosts, high availability, multi-tenancy, or RBAC;
- browser shells, container exec, host command execution, or user-supplied Compose arguments;
- general container features such as privileged mode, host namespaces, devices, or Docker socket mounts;
- arbitrary bind sources outside the host allowlist;
- automatic backup, migration, pruning, or deletion of volumes, bind data, or external networks;
- zero-downtime switching;
- rollback of database schemas, named volumes, or bind contents with a release;
- Cosign/Sigstore supply-chain signature verification for images;
- provider-specific webhook payloads or a second path from webhook directly to Docker/deployment.

## Data guarantees

Unregistering an application does not remove its container by default. Explicit removal still targets only an exact owned container. Start, stop, restart, deploy, rollback, unregister, remove, and application deletion never delete named/external volumes, bind contents, or networks.

Retention is not a business-data backup. Volumes, binds, and application databases require an independent, tested backup and restore process. A release rollback restores only the image and generated configuration; it cannot reverse a persistent-data migration.

## Documentation

- [Application model](application-model.md): configuration, resources, health, and deletion semantics.
- [Architecture](architecture.md): components, sources of truth, and persistence boundaries.
- [Deployments and rollback](deployments.md): digest releases, polling, health gates, and interrupted recovery.
- [API and streams](api-and-streams.md): authentication, idempotency, SSE, and deletion protocol.
- [Operations](operations.md) and [recovery](recovery.md): installation, backup, and incident handling.
- [Threat model](threat-model.md): Docker-root, secret, Registry, and public-entry boundaries.
- [Testing](testing.md) and [resource budget](resource-budget.md): isolated acceptance and capacity baselines.
- [Webhooks](webhooks.md): signed Registry recheck protocol.
