# SoloDock 签名 Registry Recheck Webhook

> [English](../webhooks.md)（权威版本） · 简体中文

Webhook 只表达“配置的 Registry tag 可能已变化”。它不会接收或信任 repository、tag、digest、Compose 或 Docker action；有效通知只写入 durable poll wake，随后由现有 PollCoordinator 重新读取 filesystem 配置、解析 Registry tag，并复用唯一的 digest deployment/health/rollback 状态机。

## 能力边界

Webhook 是可选增强，不是部署 API。当前不支持 provider-specific payload、unsigned compatibility、直接 deploy action、绕过 auto-deploy policy 的事件、Cosign/Sigstore、外部入口自动配置、通用 import 或多主机编排。完整 deployment 状态与 backoff/suppression 语义见 [部署与回滚](deployments.md)。

## 入口隔离

在宿主配置中为 webhook 使用独立 HTTPS origin：

```toml
webhook_public_origin = "https://solodock-hooks.example.com"
```

它必须与 `public_origin` 使用不同 authority。SoloDock 仍只监听 loopback；外部 tunnel 或 reverse proxy、DNS、TLS 和 WAF 由管理员配置。Webhook hostname 只应允许精确的 `POST /hooks/v1/apps/*/registry`，其它 path/method 均拒绝；管理 hostname 不应路由 `/hooks/**`。SoloDock 忽略 `Forwarded` 和 `X-Forwarded-*` 进行安全判断。

## v1 签名协议

请求 body 固定为 `{"event":"registry.push"}`，最大 1 KiB，`Content-Type` 必须为 `application/json`。必需 header：

- `X-SoloDock-Timestamp`: canonical Unix seconds，服务器时间前后最多 300 秒；
- `X-SoloDock-Nonce`: 16 个随机 bytes 的 base64url-no-pad；每次重试必须使用新的 nonce/timestamp；
- `X-SoloDock-Signature`: `v1=` 加 64 个 lowercase hex 字符。

签名输入为：

```text
solodock-webhook-v1\n<TIMESTAMP>\n<NONCE>\nPOST\n/hooks/v1/apps/<APP_UUID>/registry\n<SHA256_RAW_BODY_LOWER_HEX>
```

使用应用设置页生成的 32-byte base64url secret 计算 HMAC-SHA256。Secret 只显示一次，应保存到 CI secret store；不要放入 URL、命令参数或 shell history。轮换 commit 后旧 secret 立即失效，撤销只关闭 webhook，不改变周期轮询或已 durable claim 的部署。

`202 Accepted` 仅表示 recheck 已持久接受，不表示存在新 digest 或部署成功。结果应在应用 polling/deployment 页面查看。Webhook 不绕过 auto-deploy disabled、Registry backoff、busy、failed-target suppression、drift、needs-attention 或健康门禁。

管理端 webhook secret 是 write-only immutable revision，并纳入 recovery、redactor、backup 和 app deletion preview。API/幂等/删除共同边界见 [API 与实时流](api-and-streams.md)，secret 与公开入口的信任假设见 [威胁模型](threat-model.md)。
