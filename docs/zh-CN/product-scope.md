# SoloDock 产品范围

> [English](../product-scope.md)（权威版本） · 简体中文

SoloDock 是面向个人 Docker 工作负载的轻量级单机部署控制台。它以一个 Rust 进程提供管理 API 和嵌入式 Web UI，把可变镜像 tag 解析为不可变 manifest digest，并通过受限的单容器发布流程完成健康门禁、失败恢复和人工回滚。

```text
一个 SoloDock 应用 = 一个 Compose project = 一个 app service/container = 一个预构建镜像
```

SoloDock 不是通用 Docker 管理面板或完整 PaaS。严格收窄产品模型，是为了让 Docker root 级控制面仍能以可审计的字段、固定动作和精确 ownership 运行在资源有限的个人主机上。

## 目标环境

- Ubuntu 24.04 单机；
- 单管理员，不提供多租户或 RBAC；
- Docker Engine 和 Docker Compose v2.24+；
- 服务只监听 loopback，公网访问由外部 tunnel 或 reverse proxy、访问控制和 TLS 提供；
- 典型资源上限为 2 vCPU、4 GiB，控制面部署并发固定受限。

访问 Docker socket 或加入 `docker` group 在效果上等同宿主 root 权限。专用 system user、loopback 监听和 systemd hardening 是纵深防御，不是低权限隔离。

## 当前能力

SoloDock 当前支持：

- 一次性本地 bootstrap、单管理员 session、CSRF、精确 Origin 和认证审计；
- 登记多个受管单 service 应用，并为其生成 canonical Compose；
- public/write-only secret 环境变量与受管文本文件；
- loopback port、owned/external named volume、受限 bind mount 和 owned/external network；
- start、stop、restart、validate、deploy、rollback、unregister 和精确 container remove；
- filesystem-first immutable config revision、release、`active` 和 `pending`；
- public/private OCI Registry resolve、digest-only pull、多平台镜像选择和部署历史；
- 健康门禁、确定性失败恢复、unknown effect 中断保护和人工重新收敛；
- Registry polling、自动 digest 部署、no-op/coalescing/backoff 和 failed-target suppression；
- 可选的签名 Registry recheck webhook；
- 有界 events、logs、stats SSE 与 fail-closed secret redaction；
- 嵌入式生产 UI、systemd 安装、离线控制面备份/恢复和资源验收。

## 明确非目标

SoloDock 不提供：

- 源码仓库 clone、Dockerfile/buildpack 构建或 `docker compose build`；
- 任意 Compose 导入、原始 YAML 编辑、多 service、多副本或已有项目接管；
- reverse proxy、tunnel、DNS、TLS、WAF 或宿主防火墙自动配置；
- Docker Swarm、Kubernetes、多主机、高可用、多租户或 RBAC；
- 浏览器 shell、容器 exec、宿主命令执行器或用户自定义 Compose 参数；
- privileged、host namespace、device、Docker socket mount 等通用容器能力；
- 未经宿主 allowlist 授权的任意 bind source；
- 自动备份、迁移、prune 或删除 volume、bind 数据和 external network；
- 零停机切换；
- 数据库 schema、named volume 或 bind 内容随 release 回滚；
- 对镜像执行 Cosign/Sigstore 供应链签名验证；
- provider-specific webhook payload 或 webhook 直达 Docker/deployment 的第二条路径。

## 数据承诺

应用取消注册默认不移除容器；显式移除也只针对精确 owned container。start、stop、restart、deploy、rollback、unregister、remove 和 app deletion 都不删除 named/external volume、bind 内容或 network。

这种保留不等于业务备份。volume、bind 和应用数据库必须有独立、实际演练过的备份/恢复流程。release 回滚只恢复镜像与生成配置，不能撤销持久数据 migration。

## 文档导航

- [应用模型](application-model.md)：配置、资源、健康和删除语义；
- [架构](architecture.md)：组件、事实来源与持久化边界；
- [部署与回滚](deployments.md)：digest release、polling、健康门禁和中断恢复；
- [API 与流](api-and-streams.md)：认证、幂等、SSE 和删除协议；
- [运维](operations.md) 与 [恢复](recovery.md)：安装、备份和故障处理；
- [威胁模型](threat-model.md)：Docker root、secret、Registry 和公开入口边界；
- [测试](testing.md) 与 [资源预算](resource-budget.md)：隔离验收和容量基线；
- [Webhook](webhooks.md)：签名 Registry recheck 协议。
