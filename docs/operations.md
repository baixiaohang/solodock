# SoloDock 运维

生产边界先见 [产品范围](product-scope.md)；应用资源、部署状态和 system health 语义分别见 [应用模型](application-model.md)、[部署与回滚](deployments.md) 和 [API 与实时流](api-and-streams.md)。

## 安装与升级

生产目标为 Ubuntu 24.04、Docker Engine 和 Docker Compose v2.24+。先构建 Web 与嵌入式 binary：

```bash
cd web && npm ci && npm run build && cd ..
cargo build --release --locked --features embed-ui
sudo ./packaging/install.sh --version 0.1.0 --binary target/release/solodock
```

installer 使用 versioned directory 和原子 symlink，默认不启动服务、不覆盖 `/etc/solodock/config.toml` 或 `/var/lib/solodock`。完成配置和离线备份后，才显式运行 `systemctl enable --now solodock.service`；也可在首次安装使用 `--enable-now`。含新 SQLite migration 的升级是 forward-only，不能只切回旧 binary。

### 从 GitHub 构建一键升级

installer 同时安装 `/usr/local/bin/solodock-update`。先由日常管理员账号完成一次 GitHub CLI 登录，令牌只需读取仓库与 Actions artifact；不要把 token 写入脚本、配置或命令行：

```bash
gh auth login --hostname github.com
solodock-update
```

updater 会先复用已有或免密的 `sudo` 授权；需要密码时才在交互终端提示一次。无 TTY 且未配置非交互 `sudo` 的调用会在修改服务前失败。

updater 只选择目标分支最新一次成功的 `push` CI，下载该 run 重新构建并验证的 `solodock-embedded-package`，校验包内 `SHA256SUMS`，并确认 `SOURCE_SHA` 与 workflow run 的 commit SHA 完全一致。artifact 缺失、过期、校验失败或来源 commit 不匹配时，升级会在修改服务前失败。新 binary 与当前 binary 相同时不停止服务；确有更新时才停止 SoloDock，创建 `/var/backups/solodock/` 下的离线控制面备份，以 `main-<commit SHA>` 版本目录安装、启动并检查 loopback `/healthz` 与 `/favicon.svg`。临时 artifact 在所有退出路径清理，应用容器、volume 和 bind 数据不在操作范围内。

这是一项管理员显式触发的维护操作，不应直接放入无人值守 timer。新 binary 一旦被尝试启动，健康失败不会自动切回旧 binary，因为 SQLite migration 是 forward-only；此时保留备份和现场，按本页与[恢复](recovery.md)流程检查。非默认仓库、分支、workflow、备份目录或 loopback 端口可通过 `solodock-update --help` 查看参数。

## 安全前置条件

服务只监听 loopback，`public_origin` 必须是 HTTPS。外部 tunnel 或 reverse proxy、访问控制和 TLS 是部署前置条件，不由 SoloDock 配置。`solodock` 用户属于 `docker` group；这等同宿主 root 权限，必须限制主机管理员、配置文件和 Web 登录面。

启用 webhook 时还需设置不同 authority 的 `webhook_public_origin`，并在外部 WAF 只放行精确 POST path。签名、timestamp/nonce、重试和 202 语义见 [Webhook 说明](webhooks.md)。

首次启动从 `/run/solodock/bootstrap.token` 完成一次性 bootstrap。日常查看：

```bash
systemctl status solodock.service
journalctl -u solodock.service --since today
curl --fail http://127.0.0.1:8080/healthz
```

认证后的 `/api/v1/system/health` 分开展示 Docker、恢复、projection、deployment、poll coordinator、磁盘与 credential 状态。`interrupted`、`needs_attention` 或 ownership collision 需要先按 deployment detail 与精确 `docker inspect` 处理，不能 prune、宽泛删除或猜测性重跑。

任何引用 external network 的应用都必须先由管理员创建目标 Docker network。SoloDock 不改变该网络的 driver、IPAM、labels 或生命周期；升级、部署与删除也不会移除它。新服务默认使用的 `solodock-services` 不是用户 external network：SoloDock 会在首次需要时创建 internal bridge，并严格校验 `sd-services` 与 platform labels；`PLATFORM_NETWORK_IDENTITY_CONFLICT` 时应先识别同名资源来源，不能让 SoloDock 接管或自动删除。应用可用 slug 和容器端口进行内部访问。

Owned network 的 host interface 以应用详情展示值为准：旧应用可能为 `sd-<slug>`，新应用为 UUID 派生 token。配置 UFW/nftables 前先用详情页、`ip link show` 与 `docker network inspect solodock-<slug>-default` 核对身份；不要自行按 slug 猜 bridge。平台内部网络使用 `sd-services` 且是 internal，不替代应用 owned network 的出网职责。

`NETWORK_BRIDGE_IDENTITY_CONFLICT` 表示既有 owned network 的 driver 或 bridge option 与 canonical identity 不一致。SoloDock 不会删除或接管该 network；停止相关容器、核对 ownership 并由管理员处理冲突资源后，再重新部署。

应用的停机宽限默认 `10` 秒，可在注册或配置页面设置为 `1–600` 秒。它是 SIGKILL 前的最大等待，服务提前退出不会空等。需要 flush 数据、drain 队列或最终同步的应用应按自身关闭契约显式放大；deploy/recreate 停止 predecessor、手动 stop/restart、显式 remove 和失败 rollback 都使用被停止 release 的值。

## 自动部署与凭据

自动部署必须由管理员显式确认启用。开关关闭只阻止未来 poll，不取消已经 durable claim 的部署。`config_pending_manual` 表示 digest 未变但 draft 配置变化，需使用 Deploy；`suppressed_failed_target` 表示该 target 已失败/回滚，先检查 health 和数据兼容，再由新 digest/config 或明确人工部署解除。轮换 Registry credential 会改变 generation 并重新进入带 jitter 的轮询。

磁盘告警时先扩容或清理 SoloDock 之外可确认的无用内容；不得删除 state 内 revision/ledger、Docker volume 或 bind source。`MemoryHigh=256M` 是 soft pressure，没有 `MemoryMax`。

控制台 system health 的“主机内存可用”来自 Linux `/proc/meminfo` 的 `MemAvailable`，与应用容器自身的 memory usage 是两个不同事实。该解析器也被 image pull 前的 128 MiB 内存门禁复用；读取、字段或数值无效时返回 unknown 并使健康状态 degraded，不伪造为 0。

bind allow roots 在“系统设置 → 存储访问”维护。SoloDock 只验证既有绝对目录并授权应用引用，不提供目录浏览，不执行 `mkdir/chown/chmod/rm`。升级前 TOML 值只导入一次；之后应在 Web 修改。若删除被引用的 root 返回 `BIND_ROOT_IN_USE`，先从列出的 draft/active/pending 配置移除 bind 并完成安全迁移。

PostgreSQL 快速部署默认使用 major 18 和 `/var/lib/postgresql` owned volume；选择 17 时目标为 `/var/lib/postgresql/data`。升级 major 不会自动改现有 volume target或迁移数据，必须按 PostgreSQL 官方流程单独备份、迁移和验收。数据库默认不发布宿主端口，其他新服务通过 `<postgres-slug>:5432` 访问。

全局显示时区在 Web“系统设置”中从后端 IANA tzdb 列表选择，保存在 SQLite singleton settings record，默认 `UTC`。修改使用 revision、幂等键、Origin、session 与 CSRF，保存后无需重启即可重绘所有 Web 时间。该设置不向受管容器注入 `TZ`，也不改变数据库、API、SSE、cursor、过期判断或下载日志中的 UTC 原值；浏览器不支持已保存 zone 时会明确告警并按 UTC fallback。

## 备份

停止服务后执行：

```bash
sudo systemctl stop solodock.service
sudo ./packaging/solodock-backup --output /secure/new/solodock-control-plane.tar
```

archive 含应用、Registry credential 和 webhook secret，必须按高敏数据限制读取并另行加密。它保留 immutable revision 中的 network mode 与 aliases，但不包含业务 volume、bind 数据、Docker image/container 或 network；恢复前必须单独重建所需 external network，每个工作负载也必须有独立且验证过 restore 的数据备份。

恢复 archive 或处理 degraded/interrupted 状态前，按 [恢复](recovery.md) 的 fail-closed 流程操作；安全前提见 [威胁模型](threat-model.md)。
