# SoloDock 威胁模型

本模型以 [产品范围](product-scope.md) 的单主机、单管理员边界为前提。接口配额与日志脱敏行为见 [API 与实时流](api-and-streams.md)，容器资源限制见 [应用模型](application-model.md)。

## 信任边界

SoloDock 假定唯一管理员和宿主 OS 可信；Registry、镜像、容器输出、Docker/Compose 输出、浏览器输入和反向代理输入均不可信。Docker socket 和 `docker` group 在效果上等同宿主 root，因此 Web 身份认证、loopback 监听、固定动作与 systemd hardening 是纵深防御，不是对恶意宿主管理员的隔离。

secret 为 write-only：本项目拥有的 buffer 会 zeroize，secret 不进入 API response、SQLite、release、audit、argv、tracing 或错误；日志使用完整、fail-closed 的动态已知 secret 集脱敏。内核、allocator、Docker daemon、Registry 服务端和管理员业务备份不在“内存中从未出现明文”的保证范围。备份含 secret，必须高敏保护。

部署只信任严格解析并校验 digest/header/body/platform 的 Registry 结果，不验证 Cosign/Sigstore 签名。tag race 不能改变已调度 candidate，但 Registry/镜像供应链仍可能提供恶意内容。容器 capabilities、mount 和 Compose 由 typed generator 限制；bind allowlist 和 Docker data-root overlap 每次 effect 前 fail closed。

资源防护包括请求/body/stream/log buffer 上限、单一 Compose mutation、最多两个 Registry resolve、poll jitter/backoff、busy coalescing 和失败 target suppression。它不承诺抵御拥有宿主 root、Docker daemon控制权或合法管理员 session 的攻击者，也不提供多租户隔离。

Webhook hostname 是独立的公开攻击面：只信任当前 filesystem secret 对固定 body/path/timestamp/nonce 的 HMAC，不信任代理来源 IP、payload image facts 或转发 header。Nonce/wake/audit 原子持久化，body、并发和 rate map 都有固定上限；无效请求不写持久 audit/replay，以避免外部存储放大。该边界不替代外部 Tunnel/WAF rate limit。

volume、bind 和 external network 不会被自动删除，但其中数据也不会随 release 回滚。External network 与其成员属于 daemon 共享状态：SoloDock 对成员数量设置上限，在共享 deadline 内完整 inspect 成员 full ID 和有效 DNS names，任何截断或部分成功都 fail closed。Alias 冲突只对已经通过 filesystem/ownership policy 精确确认的旧 container full ID 放行；app label、名称和短 ID 不构成替换权限。

Docker observation 与后续 Compose effect 无法组成跨 API 原子事务，外部 root actor 仍可在两者之间改变网络。系统以 durable marker 后的最后一次 fresh preflight 缩小窗口，并在 effect 后用 `NETWORK_ATTACHMENT_MISMATCH` 与 `NETWORK_ALIAS_MISMATCH` 持续揭示漂移，不宣称消除有宿主 root 权限的并发。Compose 返回后的首次 container observation 以唯一非 predecessor full ID 和全套 canonical candidate-release labels 建立 ownership claim；具备 Docker daemon/root 权限的主体若在该 marker 前复制全部 canonical labels 替换容器，属于本 threat model 明确排除的主体，系统不提供因果 attestation。`post_container_id` 持久化后 exact full ID 才成为 SSOT，任何不同 ID 的 replacement 均保留现场并 fail closed。应用级备份/restore、安全更新和 schema兼容由管理员负责。

测试如何证明这些边界见 [测试与安全护栏](testing.md)；故障后的人工处置见 [恢复](recovery.md)。
