# SoloDock 运维

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

## 自动部署与凭据

自动部署必须由管理员显式确认启用。开关关闭只阻止未来 poll，不取消已经 durable claim 的部署。`config_pending_manual` 表示 digest 未变但 draft 配置变化，需使用 Deploy；`suppressed_failed_target` 表示该 target 已失败/回滚，先检查 health 和数据兼容，再由新 digest/config 或明确人工部署解除。轮换 Registry credential 会改变 generation 并重新进入带 jitter 的轮询。

磁盘告警时先扩容或清理 SoloDock 之外可确认的无用内容；不得删除 state 内 revision/ledger、Docker volume 或 bind source。`MemoryHigh=256M` 是 soft pressure，没有 `MemoryMax`。

## 备份

停止服务后执行：

```bash
sudo systemctl stop solodock.service
sudo ./packaging/solodock-backup --output /secure/new/solodock-control-plane.tar
```

archive 含应用、Registry credential 和 webhook secret，必须按高敏数据限制读取并另行加密。它不包含业务 volume、bind 数据、Docker image/container 或 network；每个工作负载必须有独立且验证过 restore 的数据备份。
