# Proposal 0002: Signed Registry Recheck Webhooks

Status: Implemented by M6.

SoloDock exposes an optional dedicated-hostname endpoint that authenticates a fixed Registry push hint with a per-app HMAC-SHA256 secret, timestamp window, and durable nonce replay protection. An accepted request atomically records its replay claim, audit event, and coalesced poll wake. It never accepts image or Docker facts from the payload and always reuses the M5 poll coordinator and M4 digest deployment state machine.

The endpoint is bounded to 1 KiB bodies, 16 concurrent requests, 120 global requests/minute, and 10 known-app requests/minute. Secrets are filesystem-first, immutable-revisioned, write-only, covered by recovery/redaction/backup, and revoked by an atomic metadata commit. See [the current webhook operations contract](../webhooks.md).

Provider-specific payloads, unsigned compatibility, direct deploy actions, webhooks that bypass auto-deploy policy, Cosign/Sigstore, Cloudflare automation, generic import, and multi-host orchestration remain out of scope.

