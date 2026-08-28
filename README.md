# SoloDock

SoloDock is a lightweight single-host container deployment console for personal Docker workloads.

> [!WARNING]
> SoloDock is in early development and is not ready to manage production workloads.

SoloDock will provide a focused Web UI for deploying prebuilt container images by immutable digest, checking health, and rolling back failed releases. It intentionally does not build source code, manage domains or TLS, provide a reverse proxy, or orchestrate multiple hosts.

## Prerequisites

- Rust stable (edition 2024) with `rustfmt` and `clippy`
- Node.js 24 and npm
- Ubuntu 24.04 and Docker Compose for the planned production environment

The current bootstrap does not access Docker. Future versions will use the Docker socket. Access to `/var/run/docker.sock`, including through membership in the `docker` group, is effectively root-equivalent and must not be treated as a security boundary.

## Backend development

Run the API scaffold:

```bash
cargo run
```

It listens on `127.0.0.1:8080` and exposes `GET /healthz`. A loopback-only override is available through `SOLODOCK_LISTEN_ADDR`:

```bash
SOLODOCK_LISTEN_ADDR=127.0.0.1:9090 cargo run
```

Run backend verification:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo build --release
```

## Frontend development

```bash
cd web
npm ci
npm run dev
```

Run frontend verification:

```bash
npm run check
npm run build
```

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
