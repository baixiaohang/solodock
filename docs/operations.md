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

## 安全前置条件

服务只监听 loopback，`public_origin` 必须是 HTTPS。Cloudflare Tunnel/WAF、访问白名单和 TLS 是外部前置条件，不由 SoloDock 配置。`solodock` 用户属于 `docker` group；这等同宿主 root 权限，必须限制主机管理员、配置文件和 Web 登录面。

启用 webhook 时还需设置不同 authority 的 `webhook_public_origin`，并在外部 WAF 只放行精确 POST path。签名、timestamp/nonce、重试和 202 语义见 [Webhook 说明](webhooks.md)。

首次启动从 `/run/solodock/bootstrap.token` 完成一次性 bootstrap。日常查看：

```bash
systemctl status solodock.service
journalctl -u solodock.service --since today
curl --fail http://127.0.0.1:8080/healthz
```

认证后的 `/api/v1/system/health` 分开展示 Docker、恢复、projection、deployment、poll coordinator、磁盘与 credential 状态。`interrupted`、`needs_attention` 或 ownership collision 需要先按 deployment detail 与精确 `docker inspect` 处理，不能 prune、宽泛删除或猜测性重跑。

任何引用 external network 的应用都必须先由管理员创建目标 Docker network。SoloDock 不改变该网络的 driver、IPAM、labels 或生命周期；升级、部署与删除也不会移除它。部署前若报告 `EXTERNAL_NETWORK_NOT_FOUND`、`NETWORK_ALIAS_CONFLICT` 或 `DOCKER_OBSERVATION_FAILED`，应在同一 Docker daemon 上用完整 container ID 检查成员和 `NetworkSettings.Networks.<name>.DNSNames`，解决冲突后重新预检；不要依赖短 ID、容器名或手工放宽 ownership。

Owned network 的 host interface 固定为 UI 显示的 `sd-<slug>`。在 Ubuntu 24.04 且 UFW routed default deny 时，可按应用精确放行 HTTPS，例如 `sudo ufw route allow in on sd-example out on <uplink> proto tcp to any port 443`；先用 `ip link show sd-example` 与 `docker network inspect solodock-example-default` 核对身份。透明代理若不应接管受管容器流量，应在 redirect/tproxy 规则之前按输入接口排除，例如 nftables 规则 `iifname "sd-example" return`。SoloDock 不修改这些宿主规则；slug 不可变且 network 重建后 interface name 保持一致。

`NETWORK_BRIDGE_IDENTITY_CONFLICT` 表示既有 owned network 的 driver 或 bridge option 与 canonical identity 不一致。SoloDock 不会删除或接管该 network；停止相关容器、核对 ownership 并由管理员处理冲突资源后，再重新部署。

## 自动部署与凭据

自动部署必须由管理员显式确认启用。开关关闭只阻止未来 poll，不取消已经 durable claim 的部署。`config_pending_manual` 表示 digest 未变但 draft 配置变化，需使用 Deploy；`suppressed_failed_target` 表示该 target 已失败/回滚，先检查 health 和数据兼容，再由新 digest/config 或明确人工部署解除。轮换 Registry credential 会改变 generation 并重新进入带 jitter 的轮询。

磁盘告警时先扩容或清理 SoloDock 之外可确认的无用内容；不得删除 state 内 revision/ledger、Docker volume 或 bind source。`MemoryHigh=256M` 是 soft pressure，没有 `MemoryMax`。

## 备份

停止服务后执行：

```bash
sudo systemctl stop solodock.service
sudo ./packaging/solodock-backup --output /secure/new/solodock-control-plane.tar
```

archive 含应用、Registry credential 和 webhook secret，必须按高敏数据限制读取并另行加密。它保留 immutable revision 中的 network mode 与 aliases，但不包含业务 volume、bind 数据、Docker image/container 或 network；恢复前必须单独重建所需 external network，每个工作负载也必须有独立且验证过 restore 的数据备份。

恢复 archive 或处理 degraded/interrupted 状态前，按 [恢复](recovery.md) 的 fail-closed 流程操作；安全前提见 [威胁模型](threat-model.md)。
