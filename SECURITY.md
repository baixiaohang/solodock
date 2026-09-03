# Security policy

## Supported versions

SoloDock is currently in the `0.x` stage. Security fixes are provided only for the latest version on the default branch. Historical commits, old build artifacts, and locally modified versions are not maintained separately.

SoloDock manages host containers through the Docker socket. Control of the SoloDock process is effectively host root access. Production deployments must keep SoloDock bound to loopback and must provide TLS, pre-authentication access control, and rate limiting at the external entry point.

## Reporting a vulnerability

Use **Report a vulnerability** on the repository Security page to submit a private report:

<https://github.com/baixiaohang/solodock/security/advisories/new>

Do not disclose an unpatched vulnerability, exploitation steps, real credentials, or production identifiers in a public Issue, Pull Request, or Discussion. Include the affected version, prerequisites, a minimal reproduction, impact, and suggested mitigations when possible.

The maintainer will make a best effort to acknowledge the report, assess its impact, and coordinate remediation and disclosure. The project does not currently promise a fixed response time. If GitHub private vulnerability reporting is unavailable, use an existing private contact channel for the maintainer and do not open a public Issue.

## Out of scope

- Capabilities that follow from an administrator deliberately granting Docker socket access, a host bind root, or a Registry credential.
- The documented single-administrator, single-host, non-multi-tenant boundaries.
- Exposure limited to version information, public configuration fields, or general deployment architecture without secrets or access capability.
- Automated scanner output without a reproducible security impact.
