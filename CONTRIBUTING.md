# Contributing to SoloDock

Thank you for improving SoloDock. Before proposing a change, read the [product scope](docs/product-scope.md), [architecture](docs/architecture.md), and [threat model](docs/threat-model.md). Preserve the single-host, single-administrator, single-service application model and its existing security boundaries.

Repository documentation and GitHub-visible collaboration use English. Simplified Chinese files under `docs/zh-CN/` and `README.zh-CN.md` are translations; the English versions are authoritative.

## Development workflow

1. Create a short-lived branch from the latest `main`.
2. Keep the change small and reviewable. Behavior changes should add deterministic tests at the lowest useful layer and update both language versions of affected documentation.
3. Run only the local validation directly related to the change. Docker E2E must use an isolated daemon or an explicit test context.
4. Open a Pull Request in English and describe the behavior, risks, security or data impact, documentation synchronization, and validation performed.
5. Wait for `ci-gate` and all other required checks, then resolve review feedback. Workflows from external forks require maintainer approval before they run.

Test concurrency must not exceed 2. Select only the commands relevant to the change:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features -- --test-threads=2

cd web
npm ci
npm run check
npm run test
npm run build
```

See [testing and safety guardrails](docs/testing.md) for Docker isolation, CI classification, and documentation-only checks.

## Security requirements

- Do not commit passwords, tokens, private keys, production hostnames/IPs, instance-specific host paths, or customer data.
- Never allow secret plaintext to enter Compose, logs, errors, audit records, ordinary API responses, or command-line arguments.
- Do not expand Docker socket access, non-loopback listening, arbitrary host binds, volume deletion, or shell/exec capabilities.
- GitHub Actions must use least privilege, and every third-party action must be pinned to a full commit SHA.
- Report vulnerabilities privately under the [security policy](SECURITY.md), not through a public Issue.

## Commit messages

Use Conventional Commits and write the type, optional scope, subject, and body in English. For example:

```text
fix(auth): reject expired sessions
```

SoloDock uses the [Apache License 2.0](LICENSE). By contributing, you confirm that you have the right to provide the contribution and agree that it will be licensed under the repository license.
