# SoloDock 恢复

> [English](../recovery.md)（权威版本） · 简体中文

本文只描述控制面与受管状态的恢复。事实来源和 publication 边界见 [架构](architecture.md)，active/pending/actual 与 deployment 终态见 [部署与回滚](deployments.md)。

## 离线恢复

只支持服务停止后的离线恢复。installer 把 `/usr/local/bin/solodock-restore` 保留为受管 symlink，其默认 validator binary 位于同一个不可变 package generation。用它校验 archive 旁的 SHA-256 文件，并且只解包到新的私有 staging directory：

```bash
sudo systemctl stop solodock.service
sudo /usr/local/bin/solodock-restore \
  --archive /secure/solodock-control-plane.tar \
  --checksum /secure/solodock-control-plane.tar.sha256 \
  --output /secure/solodock-restored
```

helper 会在解包前拒绝绝对路径、`..`、hard link、特殊文件和非预期顶层，并在任何 owner/mode 修改前校验解出的全部 link：config 必须是非 symlink 的普通文件，整个 staging 中唯一允许的 symlink 是 SoloDock 自有的 canonical `apps/<app UUID>/{active,pending} -> releases/<release UUID>`。它会解析已安装的 `solodock` 系统账户，在不跟随 symlink 的前提下把整个私有 staging tree 映射到该精确 UID/GID，再以服务身份运行版本绑定的 validator。validator 会把 canonical managed leaf 的 legacy `0400`/`0600` 规范化为 `0444`，并严格复核 owner/mode、link boundary、HMAC、config revision 与 canonical Compose。archive 的 owner 字段不会被信任或保留；意外的 `0644` mode、symlink 或特殊文件不会被自动修复。发布会锚定到已检查的父目录身份，并采用原子 no-replace 操作。不要在线覆盖现有 state。将当前 `/var/lib/solodock` 原子改名为可恢复备份，再把验证后的完整 state/config 切入：目录与普通控制面文件分别保持 `0700/0600`，`files/{public,secret}` direct leaf 保持 `0444`，最后启动并检查 journal、`/healthz` 和认证 system health。

binary、config 和 state 必须来自兼容的一组备份。SQLite migration 是 forward-only；迁移后仅回滚 binary 不安全。

## 更新失败

`solodock-update` 默认跟随当前版本绑定 `INSTALL_MANIFEST` 记录的 channel，只有管理员显式传入 `--channel` 才换轨。它会在 apply 前验证 stable Release identity 或 main CI identity、provenance attestation、source commit、package version、完整 package identity 与 checksum。preflight 失败不会改变已安装 package、服务或 state，并会清理临时下载；之后 stable 与 main 共用同一个 apply 路径。package/helper 或 channel identity 变化但 binary bytes 相同时，只安装这些资产并检查 health，不停服务也不调用 binary；binary 变化时则共用停止服务、离线备份、安装、启动、`/healthz` 与 `/favicon.svg` 路径。即使 GitHub 的 Latest Release 事实意外回退，已安装 stable manifest 也会阻止低于当前 SemVer 的自动降级。

新 binary 被调用前发生错误时，installer transaction 会把先前的 binary、helper、unit 与 manifest 作为同一个 package generation 一起恢复并验收，只有通过后 updater 才能重启旧服务。package-only 与停服更新都通过 failure injection 覆盖每个 staged asset 和公开 link commit point；独立的 rollback-operation injection 还覆盖 binary commit marker、一个 helper 与 unit。回滚不完整时会返回可区分状态，保留 generation 与 transaction 现场，并保持服务停止和给出人工恢复指引；必须先确认四个公开入口、unit 与 manifest 同属一个已验证 generation，且 `systemctl daemon-reload` 成功，才能启动服务。新 binary 一旦被调用，SQLite forward-only migration 可能已经执行，因此明确禁止自动 binary rollback。保留 updater 输出、journal、离线 `solodock-before-<version>-<timestamp>.tar` 备份及 checksum、当前 generation、`INSTALL_MANIFEST` 和失败安装现场；先诊断兼容性与 health 再选择恢复方案，不要只手工重指向一个公开 symlink 或编辑 manifest。manifest 记录 stable Release SemVer 或 main 的 `main-<commit SHA prefix>` label，generation path 还绑定完整 package identity。

## 故障分类

- SQLite 丢失：filesystem 可重建 app/release 查询事实，但管理员、session、audit、deployment history 不会被伪造，需要重新 bootstrap。
- filesystem degraded：停止 mutation，保留旧 redactor patterns；核对 owner/mode、缺失 revision 或 symlink boundary 后重启。启动恢复只会把 canonical managed leaf 的已知 legacy `0400`/`0600` 改为 `0444`；运行期 projection scan 严格只读，其他漂移需要管理员在离线备份后修复。不要递归把所有 state file 改成 `0600`，也不要手工编辑 HMAC release。
- Docker drift：用 exact project/service/full container ID 检查 unmanaged、stale 或 multiple candidate。网络 expectation 必须从实际 container release ID 对应的 active/pending immutable config revision 读取，不能拿当前 draft 覆盖；`NETWORK_ATTACHMENT_MISMATCH` 检查 network name 集，`NETWORK_ALIAS_MISMATCH` 检查期望 alias 是否为实际有效 DNS names 的子集。先备份业务数据，再明确选择人工修复；禁止通配 cleanup、`docker compose down -v`、`docker volume rm`。
- bridge drift：以应用详情投影的版本化 bridge 为期望，不能按 slug 猜测新应用的 token。核对 network ownership、driver 和 `Options.com.docker.network.bridge.name`；SoloDock 对不一致资源 fail closed 且不会自动删除。
- platform network drift：`solodock-services` 必须是 internal `bridge`、host bridge `sd-services` 并带精确 platform labels。同名 unmanaged 或漂移资源不会被接管；停止受影响发布并由管理员确认资源来源后再恢复。
- deployment `interrupted`/`needs_attention`：依据 pending/active/actual exact facts从 detail 重试或人工回滚；未知 effect 不猜测性删除。
- credential tombstone：startup/background finalizer 只在 ledger 已证明精确成功时清理；未知 marker fail closed。
- finalizer proof 缺失：不要手工移除 application/credential tombstone 或 webhook revision。周期性 idempotency cleanup 会保留完整 filesystem artifact inventory 所引用的 proof；inventory 失败时零删除。proof 缺失或格式错误仍属于 fail-closed 恢复状态。
- poll suppression：修复应用/health 后用人工 Deploy 或发布新 digest/config；不要直接改 SQLite。
- webhook degraded：保持 endpoint fail closed，修复 `webhook.toml`、immutable secret revision 的 owner/mode/HMAC 后重启；不要手工编辑 secret metadata。SQLite 丢失会丢失 nonce history/wake operational state，但不会伪造 webhook audit。

恢复后先重建所有 immutable active/pending revision 引用但 daemon 中缺失的 external network；平台网络由首次 deployment 在精确 identity 预检下重建。再逐项验证 active digest、容器 full ID、health、端口、volume/bind/network canary、platform slug DNS 与 external alias。SoloDock 的 release 回滚不回滚数据库或持久化数据。

旧 naming/config/release schema 保持可读和可回滚：旧应用继续使用原 bridge 且不会自动加入平台网络。旧 Compose schema 仍先验证 release HMAC 和它签名覆盖的精确文件 hash，再按对应 schema 校验 canonical 文档；serializer 引号表示变化不会把合法旧 release 误判为损坏，也不会放宽内容或结构校验。不要手工向历史 artifact 注入新字段；reader 会忽略旧签名域之外的控制值。SQLite global settings 备份必须与 filesystem state 同一恢复点，否则 bind roots revision 可能与应用引用不一致并使恢复 fail closed。

日常安装、备份和 health 检查入口见 [运维](operations.md)；资源保留与删除语义见 [应用模型](application-model.md)。
