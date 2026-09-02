# SoloDock API 与实时流

SoloDock 的管理 API 与嵌入式 UI 使用同一 HTTPS origin。生产进程只监听 loopback，由外部 Tunnel/WAF/TLS 提供公网入口；应用自身仍执行认证、Origin、CSRF、请求大小和权限检查。

## 认证边界

- 首次启动生成一次性 `bootstrap.token`，管理员凭据只能通过 bootstrap endpoint 初始化；
- 密码使用 Argon2id，只有一个管理员账户；
- 登录建立 Secure、HttpOnly、SameSite=Strict session cookie；
- authenticated mutation 同时要求与 `public_origin` 精确匹配的 `Origin` 和 double-submit `X-CSRF-Token`；
- session 有期限，可 logout 或 revoke-all；SSE heartbeat 也会重新验证；
- 登录限速、成功/失败及敏感管理动作写入审计，但不记录密码、cookie 或 secret。

认证协议 endpoint 不使用业务 `Idempotency-Key`。它们依靠 singleton credential、一次性 token 或随机 session 建立自己的重放边界。

## 管理 API 契约

所有 `/api/v1/**` 响应使用 `Cache-Control: no-store`，并返回 allowlist DTO，不直接序列化 Docker、Registry 或内部存储 model。错误保持稳定的 code、脱敏 message 和 `request_id`。配置校验错误还可带向后兼容的 `issues`；每项只含字段路径、稳定 code 和安全说明，不回显输入值、Secret、credential 或宿主路径：

```json
{
  "code": "APP_BUSY",
  "message": "The application already has an active mutation",
  "request_id": "...",
  "issues": [
    {
      "path": "health.http.retries",
      "code": "HEALTH_RETRIES_OUT_OF_RANGE",
      "message": "重试次数超出允许范围"
    }
  ]
}
```

持久业务 mutation 必须携带 16–128 字节安全 ASCII `Idempotency-Key`。SQLite 保存 request fingerprint/HMAC、operation 状态和脱敏响应；相同 key 与相同 request 可 replay，换 body/route/method 会冲突。Registry credential 与 webhook secret 在前端 retry identity 中只保留 hash，并在后端 API 的受管 parsed buffer 中使用 zeroizing wrapper。

接口按稳定资源分组：

- app catalog、detail、draft、validate、lifecycle 和 deletion；
- 内置 app preset 与只读 OCI image config 建议；
- Registry credential；
- deployment schedule、history、detail 和 rollback；
- per-app webhook status/configure/revoke；
- 全局显示设置 `GET/PUT /api/v1/settings`；
- system health 和 drift；
- events、logs 和 stats SSE。

具体 route、body limit 和字段以 `src/api/mod.rs`、DTO 与生成的前端类型为准。文档不复制容易漂移的完整 route 表。

`POST /api/v1/apps` 只接受 1–20 字符的不可变 `slug` 并返回 `UNCONFIGURED` 应用。首次 draft mutation 接受 `expected_revision: null`，以后必须提交精确 UUID；两种路径共享 revision/idempotency guard。Draft 输入包含 `1..=600` 的 `stop_grace_period_seconds`（缺失默认 `10`）、新 revision 默认启用的 `owned_default_network`/`service_discovery_enabled` 和结构化 external attachment。Compose 预检返回最终停机宽限、network mode、attachment、platform DNS alias、warning 与版本化资源 identity。

`GET /api/v1/app-presets` 只返回版本化公开 descriptor；`POST /api/v1/apps/from-preset` 以 write-only 变量生成正常 revision。PostgreSQL v1 支持 major 18/17，分别挂载 `/var/lib/postgresql` 与 `/var/lib/postgresql/data`，不使用 `latest`，且 response 不回显密码。Web 随后以独立稳定幂等键调用现有 deployment mutation；创建成功而部署失败时保留可恢复应用。

`POST /api/v1/images/inspect-config` 复用 Registry credential 与 manifest resolver，验证 config blob digest/大小/media type，只投影 exposed ports、volume targets、healthcheck presence、user 和 stop signal。Web 通过统一的 JSON/CSRF mutation helper 发起这个只读 POST，并原样提交当前选择的 credential reference；它不需要 durable idempotency ledger。API 不返回 image Env、labels、entrypoint/command，不写 revision；用户明确采用建议后仍走正常 draft mutation。

应用详情返回 slug 派生的 project/network/bridge 名称，并把依据实际 release identity 选择的 immutable expected network plan、expected owned identity、Docker actual driver/bridge 和 container attachment 分开展示。实际 attachment name 集不相等时报告 `NETWORK_ATTACHMENT_MISMATCH`；driver 或显式 bridge option 不一致时报告 `NETWORK_BRIDGE_IDENTITY_MISMATCH`；external attachment 缺少任一期望 alias 时报告 `NETWORK_ALIAS_MISMATCH`。不完整 inspect 不伪造 mismatch，而使 observation 保持 incomplete。

`GET /healthz` 只返回最小进程存活信息。认证后的 system health 才展示 Docker capability、filesystem recovery、projection、deployment、poll、webhook、主机 `MemAvailable`、disk、credential 和 stream 状态。Docker 不可用时认证控制面仍可启动；catalog 保留 filesystem 事实，无法完整观察的 drift 明确标记为 incomplete。

`GET /api/v1/settings` 返回 revision、显示时区、IANA 列表、动态 `allowed_bind_roots`、`slug_max_length`、支持的 mount 类型，以及由 Rust domain 定义的 `configuration_limits.health`。Web 只使用这份 capability 设置健康字段的 min/max/default；缺少 capability 时禁止保存配置。`PUT` 原子更新时区与 bind roots；被 revision 引用的 root 不得删除，扫描或 artifact 读取失败时 fail closed。设置 mutation要求 `expected_revision`、`Idempotency-Key`、精确 Origin、session 与 CSRF。显示设置只影响 Web formatter，所有 API/SSE timestamp 继续返回 RFC3339 UTC。

## 两阶段删除

删除不能只依赖 UI 当前显示。preview 在协调锁内从 fresh filesystem、verified active/pending config、webhook artifact 和 Docker observation生成 canonical facts，并签发短期 confirmation token。

DELETE 提交 token、slug 和是否移除 container；系统在 token consume 前及 filesystem tombstone前重算完整 facts hash。Network facts 包含 mode 所决定的 owned/external kind、aliases、active/pending/draft scope 和存在性，因此 attachment mode 或 alias 变化会使 token 失效。事实变化返回 stale/conflict，不执行删除。默认只 unregister，显式 remove 也只作用于 token 绑定的 exact owned container，并保留所有 volume、bind 内容和 network。

成功 tombstone 必须先从 catalog 发布移除，再精确 finalize；projection 或 fsync 不确定时保留 tombstone并由 reconciler/startup继续收敛。删除 app 会永久删除其受管 config/secret 与 webhook secret，因此 locked preview 必须明确提示。

## SSE 共同边界

events、logs 和 stats 都是 server-to-client 的 SSE，不提供 WebSocket、terminal、shell 或 exec。

StreamGate 当前限制：

| 范围 | 上限 |
| --- | ---: |
| 全局连接 | 24 |
| 单 session | 8 |
| events 全局 / 单 app | 16 / 4 |
| logs 全局 / 单 app | 8 / 2 |
| stats 全局 / 单 app | 8 / 2 |

建立流之前会重新验证 session、app catalog 和精确 full container ID ownership；响应 headers 发出前发现 Docker unavailable 或 ownership异常时返回稳定错误。15 秒 heartbeat重新验证 session，过期或 revoke-all 会关闭连接。

app deletion 先阻断新流并等待 subscriber、logs/stats producer 退出。tombstone前失败会回滚 stream generation，使仍注册的 app 可以再次订阅；commit后才永久封闭。

## Events

Docker events 只投影匹配 SoloDock ownership 的 allowlist字段。每进程 boot UUID 与单调 sequence构成 cursor，有限 ring支持同一进程内 replay；无法 replay 时发送 reset，而不是伪造连续历史。慢消费者超过有界 queue 会收到 `SLOW_CONSUMER` 并断开。

## Logs

logs 支持有限 tail/since cursor，不接受任意 Docker 参数。framer 先重组 logical line，再按 byte对 SoloDock 已知 secret做单遍有界脱敏，跨 Docker chunk 的 pattern仍能覆盖。

- 原始行超过 64 KiB 时整行省略；
-正常 message 上限 16 KiB；
-移除 NUL 和终端控制序列；
-输出只包含 stream、timestamp、message 等 allowlist字段。

控制面从未持有的应用自生成 secret 无法可靠识别，不在脱敏保证内。

## Stats

stats 只在有 subscriber 时创建 Docker producer，并只保留最新 sample，不累计无界历史。最后一个 subscriber 离开后取消 producer；独立 producer registry保证 deletion/shutdown 能 cancel并 join 所有 generation。

静态资源、SPA fallback 和安全 header 由 embedded asset handler提供；`/api/**` 和 `/hooks/**` 不进入 SPA fallback。公开 webhook 的独立 Host 与签名协议见 [Webhook](webhooks.md)。
