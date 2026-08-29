# SoloDock 当前架构

SoloDock 是单主机、单管理员、单 service 的容器部署控制面。应用、不可变 config revision、Registry credential metadata/secret revision 和 release 位于私有文件系统；`active` symlink 是 active release 的唯一事实。Docker daemon 是容器实际状态的事实源，Registry 是 tag 当前指向的事实源。SQLite 保存认证、审计、幂等、部署 ledger 与可丢失的 polling operational state，不反向覆盖文件系统事实。

## 发布与自动部署

所有 manual、rollback 和 poll trigger 进入同一 `DeploymentScheduler` 与 `DeploymentEngine`。tag 仅用于 Registry resolve；调度行原子保存本机平台的 manifest digest，随后 candidate release 在任何 Docker effect 前落盘并设置 `pending`。固定 Compose runner 只运行 digest-pinned 单 service；精确容器通过健康门禁后才切换 `active`。未知 effect 保持 `interrupted`，确定性失败才恢复旧 active。

一个进程级 `PollCoordinator` 管理 enabled app 的有界 due heap，Registry resolve 最多并发 2。generation 覆盖 draft、source、credential revision、开关和 interval；在途结果在调度前重新读取 filesystem 与 Docker facts。busy 不排队，相同 active digest 不部署，仅 config 变化标为人工处理，失败 target 会在 SQLite 中抑制到 target/generation 改变。

## HTTP 与数据边界

生产 binary 通过 `embed-ui` 编译期嵌入 Vite 产物。`/api/v1/**` 保持认证、Origin/CSRF 与 `no-store`；`/healthz` 仅提供最小存活信息。hashed asset 长缓存，HTML 不缓存，并统一设置 CSP、frame、referrer、MIME 与 permissions 安全 header；API 404 不进入 SPA fallback。

取消注册和移除容器均保留 named/external volume、bind 内容和 network。bind 默认禁用，只能使用管理员 allowlist 的严格子目录，并在每次 Docker effect 前重新验证路径和 Docker data-root。

另见 [运维](operations.md)、[恢复](recovery.md) 与 [威胁模型](threat-model.md)。
