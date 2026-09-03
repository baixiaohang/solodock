# SoloDock signed Registry recheck webhook

> English (authoritative) · [简体中文](zh-CN/webhooks.md)

A webhook states only that a configured Registry tag may have changed. It neither receives nor trusts a repository, tag, digest, Compose document, or Docker action. A valid notification writes only a durable poll wake. The existing `PollCoordinator` then reloads filesystem configuration, resolves the Registry tag, and uses the sole digest-deployment, health, and rollback state machine.

## Capability boundary

The webhook is an optional enhancement, not a deployment API. It does not support provider-specific payloads, unsigned compatibility, direct deploy actions, events that bypass auto-deploy policy, Cosign/Sigstore, automatic external-entry configuration, general import, or multiple hosts. See [deployments and rollback](deployments.md) for complete deployment-state and backoff/suppression semantics.

## Entry-point isolation

Configure a separate HTTPS origin for webhooks:

```toml
webhook_public_origin = "https://solodock-hooks.example.com"
```

It must use a different authority from `public_origin`. SoloDock still listens only on loopback; the administrator configures the external tunnel or reverse proxy, DNS, TLS, and WAF. The webhook hostname should allow only exact `POST /hooks/v1/apps/*/registry`; reject every other path and method. The management hostname should not route `/hooks/**`. SoloDock ignores `Forwarded` and `X-Forwarded-*` for security decisions.

## v1 signature protocol

The request body is exactly `{"event":"registry.push"}`, at most 1 KiB, with `Content-Type: application/json`. Required headers are:

- `X-SoloDock-Timestamp`: canonical Unix seconds, within 300 seconds before or after server time;
- `X-SoloDock-Nonce`: 16 random bytes encoded as base64url-no-pad; each retry needs a new nonce and timestamp;
- `X-SoloDock-Signature`: `v1=` followed by 64 lowercase hexadecimal characters.

The signature input is:

```text
solodock-webhook-v1\n<TIMESTAMP>\n<NONCE>\nPOST\n/hooks/v1/apps/<APP_UUID>/registry\n<SHA256_RAW_BODY_LOWER_HEX>
```

Compute HMAC-SHA256 with the 32-byte base64url secret generated on the application settings page. The secret is shown once. Save it in a CI secret store; never place it in a URL, command argument, or shell history. A committed rotation invalidates the previous secret immediately. Revocation disables only the webhook and does not change periodic polling or an already durable deployment claim.

`202 Accepted` means only that the recheck was durably accepted, not that a new digest exists or a deployment succeeded. Inspect results in the application's polling/deployment page. Webhooks never bypass auto-deploy-disabled, Registry backoff, busy, failed-target suppression, drift, needs-attention, or health-gate policy.

The management-side webhook secret is a write-only immutable revision included in recovery, redaction, backup, and application deletion preview. See [API and streams](api-and-streams.md) for shared API/idempotency/deletion boundaries and the [threat model](threat-model.md) for trust assumptions about secrets and public entry points.
