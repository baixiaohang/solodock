# Documentation authoring and maintenance

This file applies to the entire `docs/` tree and independently governs documentation changes. When the repository root contains project-wide instructions, its development, security, Git, and language rules also apply.

## Language, pairing, and format

- English is the authoritative documentation language. Keep authoritative topic documents at `docs/<name>.md` and their Simplified Chinese translations at `docs/zh-CN/<name>.md` with identical relative paths and filenames.
- Every English topic document under `docs/` must have a Simplified Chinese counterpart, and every Simplified Chinese topic document must map to an English authoritative document. `AGENTS.md` and the `CLAUDE.md` symlink are instruction files, not topic documents; do not create localized copies of them.
- Put a language switch at the top of every paired document. State that the English version is authoritative. English pages link to `zh-CN/<name>.md`; Chinese pages link to `../<name>.md`.
- Changes to behavior, APIs, configuration, operations, security, or test guardrails must update both language versions in the same Pull Request. A translation-only wording correction may change only the Chinese file when it does not alter meaning.
- If the two versions disagree, fix the translation; the English version remains authoritative.
- Use `kebab-case.md` for ordinary document filenames. `AGENTS.md` and `CLAUDE.md` are conventional exceptions. Prefer relative links between documents.
- Preserve canonical code identifiers, commands, environment variables, API paths, error codes, and product names in both languages.
- Use diagrams, directory trees, or data examples only when they materially aid understanding. Avoid copying large implementation details that will drift from the code.
- SoloDock uses Apache-2.0. Documentation must not include instance-specific host paths, network identifiers, deployment credentials, or other private operational data.

## Directory responsibilities and lifecycle

- `proposals/` holds designs that have not yet been fully implemented and confirmed. During implementation, an approved proposal is the design contract for product boundaries and acceptance criteria.
- After a feature is implemented, move still-valid architecture, invariants, and operational requirements into the paired topic documents. Do not leave a completed proposal as the permanent description of current behavior.
- Do not add "patch documents" for the same fact. Correct the authoritative documentation or implementation and remove stale statements.

## Sources of truth

- Code, tests, configuration schema, and migrations define current implemented behavior.
- Maintainer-approved requirements and the current approved proposal define the intended product boundary until the maintainer approves a replacement.
- If implementation and intended design conflict, do not choose one silently or hide the mismatch in documentation. Fix the implementation, or obtain maintainer approval before updating design and implementation in the same Pull Request.
- Topic documents describe current behavior. Historical proposals, external projects, and historical material provide context only; they do not define SoloDock behavior or product scope.

## Content and synchronization

- Document product positioning, non-goals, design tradeoffs, state machines, security boundaries, failure semantics, recovery requirements, and test guardrails.
- When architecture, API fields, configuration, storage layout, deployment/rollback semantics, or security boundaries change, update the relevant English and Simplified Chinese topic documents in the same Pull Request.
- When commands or configuration can be derived exactly from source, schema, or `--help`, document only the necessary entry points and semantics instead of duplicating full generated output.
- Verify new external links. Identify whether an external reference is background research or a normative source.
