# SoloDock

**一个聚焦于在单台个人 Docker 主机上安全运行预构建 OCI 镜像的部署控制台。**

[English（权威版本）](README.md) · [文档](docs/zh-CN/product-scope.md) · [贡献指南](CONTRIBUTING.md) · [安全策略](SECURITY.md)

> [!WARNING]
> SoloDock 仍处于 `0.x` 单机阶段。Docker socket 等同宿主 root 权限；部署前必须为业务数据建立独立备份，并加固外部访问入口。

SoloDock 将镜像 tag 解析为 digest 固定的 release，应用系统生成的单 service Compose 配置，等待所选健康策略通过后才提交 release。candidate 确定性失败时，它会精确移除 candidate，或恢复并重新验证上一 release。人工回滚也经过同一条受保护的部署路径。

SoloDock 不构建源码、不接受任意 Compose、不管理域名/TLS、不提供反向代理，也不编排多台主机。它刻意比通用 Docker 管理器或自托管 PaaS 更窄。

Web UI 在登录前后都支持 English 与简体中文。首次访问时，只有浏览器第一偏好语言为 `zh` 或 `zh-*` 才选择简体中文，否则使用 English；显式选择只记忆在当前浏览器中。

![SoloDock 英文登录界面](docs/assets/solodock-login-en.png)

## 部署链路

```text
GitHub Actions 或其他构建器
        │ 推送预构建镜像
        ▼
OCI Registry
        │ tag 解析为平台特定 digest
        ▼
SoloDock
        │ 生成 Compose + 健康门禁 + 失败恢复
        ▼
单台 Docker 主机
```

## 为什么是 SoloDock

- **Digest 固定的 release：** tag 只用于发现；已部署 release 和生成的 Compose 使用不可变 manifest digest。
- **健康门禁后提交：** candidate 只有通过配置的 `healthy`、`running`、`completed`，或明确降低安全性的 `disabled` 策略后才成为 `active`。
- **失败恢复：** candidate 确定性失败时执行精确清理，或重新应用并 fresh 验证上一 release。未知 effect 会保留现场并进入 `interrupted` 或 `needs_attention`，不会猜测。
- **人工回滚：** 通过相同的身份、资源和健康检查重新创建较早的不可变镜像/配置 release。
- **保守 ownership：** Docker effect 必须通过 project、service、application、release、schema 和 full container ID 精确校验。volume、bind 数据和 network 均保留。

回滚只恢复 release 镜像与生成配置，不回滚数据库 migration、named volume 或 bind 内容；SoloDock 也不提供零停机切换。

## 适合谁

SoloDock 可能适合以下情况：

- 由一个管理员在一台 Ubuntu 服务器上运行个人服务；
- 已经在 CI 中构建镜像并推送到 OCI Registry；
- 希望用聚焦的 Web 流程完成配置、部署、健康验证、digest 自动轮询、失败恢复和回滚；
- 已经维护 HTTPS tunnel 或 reverse proxy，以及工作负载自己的备份；
- 更偏好 typed fields 和系统生成的 Compose，而不是任意 YAML。

以下需求不属于目标范围：

- 源码构建、buildpack 或基于 Git 的应用构建；
- 任意 Compose stack、单应用多 service、副本或接管已有项目；
- 内置 proxy、DNS、证书、防火墙管理或零停机路由；
- 多主机、Kubernetes/Swarm、高可用、团队、多租户或 RBAC；
- 浏览器 shell/exec、privileged container、任意 host bind 或自动数据迁移/备份。

完整边界见[产品范围](docs/zh-CN/product-scope.md)。

## 与相邻项目的边界

以下项目解决相邻问题，但采用不同的运行模型。这里比较的是产品范围，不是宣称某个项目普遍更优。

| 项目 | 主要模型 | 与 SoloDock 的边界差异 |
| --- | --- | --- |
| SoloDock | 单机上每个应用一个 typed、预构建镜像 service | digest 固定 release、健康门禁提交、受保护的失败恢复；不支持任意 Compose、构建、proxy 或多主机控制 |
| [Dockge](https://github.com/louislam/dockge) | Compose stack 管理 | 以编辑和运行 Compose stack 为中心，而不是 SoloDock 生成的单 service release 模型 |
| [Watchtower](https://github.com/containrrr/watchtower) | 自动更新运行中的 container | 聚焦镜像更新自动化，而不是带不可变 release 历史和人工回滚的应用配置控制台 |
| [Coolify](https://github.com/coollabsio/coolify) | 更广泛的自托管平台 | 包含更完整的应用/平台流程；SoloDock 刻意排除源码构建、集成 proxy/TLS 和多服务器编排 |

## 从源码构建并安装

可长期下载、可验证的 GitHub Release asset 尚在准备中。当前真实安装路径要求构建机具有 Rust stable、Node.js 24、npm 和 Git；生产主机必须是 Ubuntu 24.04，并安装 Docker Engine、`docker` group/socket、systemd 和 Docker Compose v2.24+。

```bash
git clone https://github.com/baixiaohang/solodock.git
cd solodock
cd web && npm ci && npm run build && cd ..
cargo build --release --locked --features embed-ui
sudo ./packaging/install.sh --version 0.1.0 --binary target/release/solodock
```

installer 会创建专用 `solodock` system account，安装 systemd unit 和示例 `/etc/solodock/config.toml`，并保留已有配置与 state。默认不会启动服务。

首次启动前：

1. 编辑 `/etc/solodock/config.toml`，保持 `listen_address` 只监听 loopback，并把 `public_origin` 设置为外部实际提供的精确 HTTPS origin。
2. 配置带 TLS、认证前访问控制和限速的外部 tunnel 或 reverse proxy。
3. 为每个工作负载的 volume、bind 和数据库建立独立备份。
4. 启动服务，并在不写入日志的前提下读取一次性 bootstrap token：

```bash
sudo systemctl enable --now solodock.service
sudo systemctl status solodock.service
sudo cat /run/solodock/bootstrap.token
```

打开配置的 `public_origin` 完成 bootstrap，然后创建应用。升级、备份、认证和故障处理的准确要求见[运维](docs/zh-CN/operations.md)与[恢复](docs/zh-CN/recovery.md)。当前 `solodock-update` 使用会过期、带 attestation 的 `main` workflow artifact，属于开发 channel，而不是稳定 release channel。

## 安全前提

- `/var/run/docker.sock` 与 `docker` group 权限等同宿主 root，不是低权限边界。
- SoloDock 和应用发布端口只接受 loopback listener。公网访问必须由外部 HTTPS tunnel 或 reverse proxy 与访问控制提供。
- SoloDock 保留 volume 和 bind 内容，但不会备份它们，也不会在 release 回滚时撤销数据 migration。
- Registry token 与 webhook secret 是 write-only，并且不进入普通 API、Compose、日志或审计；离线控制面备份则包含 secret，必须按高敏数据处理。
- SoloDock 校验 Registry digest 与平台身份，但当前不验证 Cosign/Sigstore 镜像签名。

生产使用前请阅读[威胁模型](docs/zh-CN/threat-model.md)。安全漏洞应通过[私密漏洞报告](SECURITY.md)提交，不要创建公开 Issue。

## 文档

- 产品与配置：[产品范围](docs/zh-CN/product-scope.md)、[应用模型](docs/zh-CN/application-model.md)
- 系统与发布语义：[架构](docs/zh-CN/architecture.md)、[部署与回滚](docs/zh-CN/deployments.md)、[API 与实时流](docs/zh-CN/api-and-streams.md)
- 生产运行：[运维](docs/zh-CN/operations.md)、[恢复](docs/zh-CN/recovery.md)、[威胁模型](docs/zh-CN/threat-model.md)、[资源预算](docs/zh-CN/resource-budget.md)
- 专题与验收：[Webhook](docs/zh-CN/webhooks.md)、[测试与安全护栏](docs/zh-CN/testing.md)

专题文档描述当前实现。Git 历史保留已完成的设计和交付计划，但它们不是当前行为的事实来源。中英文出现冲突时，以英文权威版本为准。

## 贡献与许可证

提交改动前请阅读 [CONTRIBUTING.md](CONTRIBUTING.md)。SoloDock 使用 [Apache License 2.0](LICENSE)。
