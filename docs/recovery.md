# SoloDock 恢复

本文只描述控制面与受管状态的恢复。事实来源和 publication 边界见 [架构](architecture.md)，active/pending/actual 与 deployment 终态见 [部署与回滚](deployments.md)。

## 离线恢复

只支持服务停止后的离线恢复。先校验 archive 旁的 SHA-256 文件，在私有临时目录解包并拒绝绝对路径、`..`、hard link、特殊文件和非预期顶层。唯一允许的 symlink 是 SoloDock 自有的 canonical `apps/<app UUID>/{active,pending} -> releases/<release UUID>`；restore helper 会在交付 staging 前调用同 package binary 的只读 recovery validator，复核 owner/mode、link boundary、HMAC、config revision 与 canonical Compose。不要在线覆盖现有 state。将当前 `/var/lib/solodock` 原子改名为可恢复备份，再把验证后的完整 state/config 切入，修正 `solodock:solodock` ownership 与 `0700/0600` mode，最后启动并检查 journal、`/healthz` 和认证 system health。

binary、config 和 state 必须来自兼容的一组备份。SQLite migration 是 forward-only；迁移后仅回滚 binary 不安全。

## 故障分类

- SQLite 丢失：filesystem 可重建 app/release 查询事实，但管理员、session、audit、deployment history 不会被伪造，需要重新 bootstrap。
- filesystem degraded：停止 mutation，保留旧 redactor patterns；修复 owner/mode、缺失 revision 或 symlink boundary 后重启/等待 projection reconciler。不要手工编辑 HMAC release。
- Docker drift：用 exact project/service/full container ID 检查 unmanaged、stale 或 multiple candidate。网络 expectation 必须从实际 container release ID 对应的 active/pending immutable config revision 读取，不能拿当前 draft 覆盖；`NETWORK_ATTACHMENT_MISMATCH` 检查 network name 集，`NETWORK_ALIAS_MISMATCH` 检查期望 alias 是否为实际有效 DNS names 的子集。先备份业务数据，再明确选择人工修复；禁止通配 cleanup、`docker compose down -v`、`docker volume rm`。
- bridge drift：以应用详情中的 `solodock-<slug>-default` / `sd-<slug>` 为期望，核对 network ownership、driver 和 `Options.com.docker.network.bridge.name`。SoloDock 对不一致资源 fail closed 且不会自动删除；修复后重建 network 仍使用相同 bridge identity。
- deployment `interrupted`/`needs_attention`：依据 pending/active/actual exact facts从 detail 重试或人工回滚；未知 effect 不猜测性删除。
- credential tombstone：startup/background finalizer 只在 ledger 已证明精确成功时清理；未知 marker fail closed。
- poll suppression：修复应用/health 后用人工 Deploy 或发布新 digest/config；不要直接改 SQLite。
- webhook degraded：保持 endpoint fail closed，修复 `webhook.toml`、immutable secret revision 的 owner/mode/HMAC 后重启；不要手工编辑 secret metadata。SQLite 丢失会丢失 nonce history/wake operational state，但不会伪造 webhook audit。

恢复后先重建所有 immutable active/pending revision 引用但 daemon 中缺失的 external network，再逐项验证 active digest、容器 full ID、health、端口、volume/bind/network canary 与 external alias。External-only revision 不应出现应用 owned default network；SoloDock 的 release 回滚不回滚数据库或持久化数据。

本版本是有意的破坏性 schema 切换：旧 UUID-based app header、release marker 与 Compose artifact 不兼容，也没有 startup migration 或 fallback。升级前应停止服务、备份后清空旧 SoloDock app state，并用 1–12 字符 slug 重新登记；不要手工改写带 HMAC 的 header/release。

日常安装、备份和 health 检查入口见 [运维](operations.md)；资源保留与删除语义见 [应用模型](application-model.md)。
