# SoloDock 威胁模型

> [English](../threat-model.md)（权威版本） · 简体中文

本模型以 [产品范围](product-scope.md) 的单主机、单管理员边界为前提。接口配额与日志脱敏行为见 [API 与实时流](api-and-streams.md)，容器资源限制见 [应用模型](application-model.md)。

## 信任边界

SoloDock 假定唯一管理员和宿主 OS 可信；Registry、镜像、容器输出、Docker/Compose 输出、浏览器输入和反向代理输入均不可信。Docker socket 和 `docker` group 在效果上等同宿主 root，因此 Web 身份认证、loopback 监听、固定动作与 systemd hardening 是纵深防御，不是对恶意宿主管理员的隔离。

Reverse proxy 提供的 authority 输入不可信。SoloDock 将 management 路由绑定到 canonical `public_origin` authority，并且只允许独立 webhook authority 访问唯一 canonical POST path。未知、缺失、重复、畸形或 URI/`Host` 冲突的 authority 会在 API 或 asset 路由前 fail closed；forwarding header 与来源 IP 都不会扩展任一 surface。独立 loopback authority 仅暴露两个非敏感 updater probe，不暴露 login、API、SSE、UI 或 webhook 行为。

官方 package state 仅允许固定的 config/state/runtime 布局。Runtime、installer、updater、backup 与 restore 都会在各自 durable boundary 之前调用同一个 Rust layout validator；shell 不跟随任意 TOML 路径。Docker socket 缺失是允许的 degraded availability 状态，但已存在却类型或 group 错误的路径会被拒绝。Systemd soft dependency 可避免 Docker outage/restart 停止与 Docker 无关的控制面访问，但不会降低可用 Docker socket 实际等同 host root 的权限。

secret 为 write-only：本项目拥有的 buffer 会 zeroize，secret 不进入 API response、SQLite、release、audit、argv、tracing 或错误；日志使用完整、fail-closed 的动态已知 secret 集脱敏。内核、allocator、Docker daemon、Registry 服务端和管理员业务备份不在“内存中从未出现明文”的保证范围。备份含 secret，必须高敏保护。

仅持有合法管理员 session 不能轮换 credential：该 route 还要求当前密码、精确 Origin 与 CSRF proof。无效 current-password 尝试与 login 共用 throttle，但使用独立的脱敏 audit action 和 HTTP 403 结果，因此不会伪装成 session 过期，也不泄露密码材料。轮换与 login 串行化，并在原子替换 hash 时撤销全部 session。这让密码泄露后的止损依赖一次成功轮换；本次不增加 MFA、account recovery、password history，也不增加用于取消 commit 前已获准 request 的 authentication epoch。

受管 public/secret file 的宿主 leaf 为 `0444`，用于让显式 bind mount 的非 root 容器读取；其全部宿主 ancestor 仍为 `0700 solodock:solodock`，容器只看到 Compose 明确挂入的单个 leaf。该边界不隔离宿主 root、Docker daemon 控制者或拥有 Docker socket 的主体，也不允许受管容器浏览完整 state tree。只读性由 Compose `read_only: true` 和 immutable revision 共同执行；其他控制面 secret 不因该例外放宽。

受管 state reader 在拼接或读取 leaf 前先验证来自 metadata 的文件名；root-relative 路径只接受普通组件，拒绝 `.`、`..`、absolute/prefix 和 symlink boundary。HMAC 负责内容完整性，不被当作延迟执行的路径消毒器；路径不合法时必须在读取目标内容前 fail closed。

部署只信任严格解析并校验 digest/header/body/platform 的 Registry 结果，不验证 Cosign/Sigstore 签名。tag race 不能改变已调度 candidate，但 Registry/镜像供应链仍可能提供恶意内容。容器 capabilities、mount 和 Compose 由 typed generator 限制；bind allowlist 和 Docker data-root overlap 每次 effect 前 fail closed。

拥有 read-write bind 的受管容器在该 source 范围内属于不可信宿主文件系统 writer。因此，SoloDock 会拒绝这个 source 成为另一 bind source 严格祖先的任何计划。同一应用替换时，系统先停止并确认 exact writer，再 fresh 解析和复核 bind；另一应用中的冲突 writer 会阻止 start-like action，且绝不会被自动停止。这关闭了受管 SIGTERM path-swap 窗口，但不承诺抵御 host root 或能并发修改 allowed path 的独立进程。

资源防护包括请求/body/stream/log buffer 上限、单一 Compose mutation、最多两个 Registry resolve、poll jitter/backoff、busy coalescing 和失败 target suppression。它不承诺抵御拥有宿主 root、Docker daemon控制权或合法管理员 session 的攻击者，也不提供多租户隔离。

Webhook hostname 是独立的公开攻击面：在 HMAC 处理前，其 authority 只接受 `POST /hooks/v1/apps/<canonical-lowercase-UUID>/registry`，management、local probe 与 webhook surface 不会交叉路由；之后只信任当前 filesystem secret 对固定 body/path/timestamp/nonce 的 HMAC，不信任代理来源 IP、payload image facts 或转发 header。Nonce/wake/audit 原子持久化，body、并发和 rate map 都有固定上限；无效请求不写持久 audit/replay，以避免外部存储放大。该边界不替代外部 Tunnel/WAF rate limit。

volume、bind 和 external network 不会被自动删除，但其中数据也不会随 release 回滚。External network 与其成员属于 daemon 共享状态：SoloDock 对成员数量设置上限，在共享 deadline 内完整 inspect 成员 full ID 和有效 DNS names，任何截断或部分成功都 fail closed。Alias 冲突只对已经通过 filesystem/ownership policy 精确确认的旧 container full ID 放行；app label、名称和短 ID 不构成替换权限。

Slug 是受限、全局唯一且不可变的人类可读命名空间，但不是 ownership 凭据；UUID app label 仍决定资源归属。Owned network 复用前必须同时匹配 exact name、UUID/project labels、bridge driver 与固定 option，防止同名 unmanaged network 或错误 host interface 被接管。用户不能输入任意 project、container、network 或 bridge name。

Docker observation 与后续 Compose effect 无法组成跨 API 原子事务，外部 root actor 仍可在两者之间改变网络。系统以 durable marker 后的最后一次 fresh preflight 缩小窗口，并在 effect 后用 `NETWORK_ATTACHMENT_MISMATCH` 与 `NETWORK_ALIAS_MISMATCH` 持续揭示漂移，不宣称消除有宿主 root 权限的并发。Compose 返回后的首次 container observation 以唯一非 predecessor full ID 和全套 canonical candidate-release labels 建立 ownership claim；具备 Docker daemon/root 权限的主体若在该 marker 前复制全部 canonical labels 替换容器，属于本 threat model 明确排除的主体，系统不提供因果 attestation。`post_container_id` 持久化后 exact full ID 才成为 SSOT，任何不同 ID 的 replacement 均保留现场并 fail closed。应用级备份/restore、安全更新和 schema兼容由管理员负责。

测试如何证明这些边界见 [测试与安全护栏](testing.md)；故障后的人工处置见 [恢复](recovery.md)。
