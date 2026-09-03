# AGENTS.md

SoloDock is a lightweight Docker application deployment console for personal, single-host environments. One SoloDock application maps to one container and one prebuilt image. SoloDock generates a minimal Compose configuration; it does not build source code, import arbitrary Compose files, provide a reverse proxy, or orchestrate multiple hosts.

## Language and documentation

- Use English for repository documentation and all GitHub-visible collaboration, including Issues, Pull Requests, Reviews, Discussions, Release Notes, and changelog entries.
- Follow Conventional Commits. Write the type, optional scope, subject, and body in English, for example `feat: add application status query`.
- Keep code identifiers, commands, environment variables, API paths, error codes, and third-party product names in their canonical form. Write new or substantially modified code comments and user-facing text in English.
- Simplified Chinese translations are allowed only in explicitly localized files such as `README.zh-CN.md` and `docs/zh-CN/*.md`. English documentation is authoritative if translations disagree.
- Direct maintainer conversations, private design confirmation, and internal coordination may use the participants' preferred language; this does not change the repository language policy.
- When architecture, APIs, configuration, operations, security boundaries, or test guardrails change, update the corresponding English and Simplified Chinese documentation in the same Pull Request. See `docs/AGENTS.md` for the documentation pairing rules.
- Do not rewrite Git history or translate unrelated legacy comments solely for language consistency. Apply this policy to new content and files otherwise changed by the task.

## Stack and common commands

- Backend: Rust stable, edition 2024, Axum, and Tokio.
- Frontend: Svelte, TypeScript, and Vite. Production builds embed the frontend in a single Rust binary; no Node service runs in production.

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features -- --test-threads=2

cd web
npm ci
npm run check
npm run build
```

Run the smallest validation directly related to the change. Do not run the complete suite or host Docker E2E unless the maintainer explicitly requests it.

## Development and security boundaries

- Preserve the single-process, single-Rust-crate, single-service application model. Do not introduce platform abstractions for hypothetical future features.
- Access to the Docker socket or membership in the `docker` group is effectively host root access. Never describe it as a low-privilege security boundary or expose it to managed containers or the Web API.
- Bind the management endpoint and published application ports only to loopback addresses. The MVP does not accept non-loopback listeners or port mappings.
- Secret plaintext must never enter Git, Compose files, logs, errors, audit records, ordinary API responses, or command-line arguments. Compose may only reference managed secrets.
- Application deletion preserves volumes by default. Never use `docker system prune`, wildcard deletion, or destructive commands without exact verification.
- Docker/Compose integration tests must use an isolated daemon or an explicit test context, random projects, dedicated labels, and exact-ID cleanup.

## Change conventions

- Keep changes small and reviewable. Do not commit generated output such as `target/`, `web/node_modules/`, or `web/dist/`.
- Commit Rust and npm lockfiles together with dependency changes.
- Add deterministic tests at the lowest useful layer for new behavior. Prioritize failure paths, security boundaries, and destructive operations.
- Do not expand the product boundary or perform volume/data migrations without maintainer approval for the specific design.
