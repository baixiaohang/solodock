## Summary

<!-- What changes, and why is it needed? -->

## Behavior and scope

<!-- Describe user-visible behavior and confirm that the focused product boundary is unchanged, or link the approved design for a boundary change. -->

## Risk, security, and data impact

<!-- Cover Docker access, secrets, networking, storage, deletion, backup, rollback, migrations, and failure behavior as applicable. Write "None" only after checking them. -->

## Documentation

- [ ] I updated the authoritative English documentation for any behavior, API, configuration, operations, security, or test-guardrail change.
- [ ] I updated the matching `docs/zh-CN/` translation in the same Pull Request, or this change does not affect paired topic documentation.

## Validation

<!-- List the exact commands and manual checks run. -->

- [ ] `git diff --check`
- [ ] Relevant deterministic tests or checks
- [ ] Docker E2E used an isolated daemon or explicit test context, or was not required

## Checklist

- [ ] GitHub-visible text and commit messages are in English.
- [ ] No credentials, private host details, generated build output, or unrelated changes are included.
- [ ] Destructive operations preserve SoloDock's exact-ownership and data-retention boundaries.
