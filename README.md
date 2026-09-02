# SoloDock

SoloDock 是面向个人 Docker 工作负载的轻量级单机容器部署控制台。

> [!WARNING]
> SoloDock 是单主机 MVP。Docker socket 等同宿主 root；部署前必须完成独立业务数据备份和访问面硬化。

SoloDock 提供聚焦的 Web 界面，用不可变镜像 digest 部署预构建容器镜像、检查应用健康状态，并在新版本失败时恢复旧 release。它不构建源码、不管理域名或 TLS、不提供反向代理，也不编排多台主机。

新建空白服务只需填写全局唯一、创建后不可修改的 1–20 字符服务名；服务会以 `UNCONFIGURED` 状态登记，随后再在详情页逐行配置镜像、端口、存储、健康检查和受管文件。SoloDock 仍以 UUID 作为 API、目录和 ownership label 的安全身份，并通过版本化 naming helper 派生 Docker 资源。旧应用保留原 `sd-<slug>` bridge，新应用使用 UUID 派生的稳定短 bridge，升级不会重命名已有资源。

SoloDock 采用 [Apache License 2.0](LICENSE) 开源。

## 环境要求

- Rust stable（edition 2024），包含 `rustfmt` 和 `clippy`
- Node.js 24 和 npm
- 生产目标环境为 Ubuntu 24.04、Docker Engine 和 Docker Compose v2.24+

生产 Docker observer 通过固定的 `/var/run/docker.sock` 观察 Docker Engine，不接受 `DOCKER_HOST`、TCP endpoint 或自定义 socket 配置。访问 Docker socket（包括通过 `docker` group）在效果上等同宿主 root 权限，不能把它视为安全边界。Docker 不可用时认证控制面仍会启动，应用目录和系统健康 API 返回 degraded 状态；logs、stats 和 events stream 会在建立响应前返回稳定的 `503`。

## 配置与后端开发

SoloDock 只从 `SOLODOCK_CONFIG_PATH` 指定的 TOML 文件加载宿主配置，未设置时使用 `/etc/solodock/config.toml`。配置必须使用 loopback IP 监听地址和 HTTPS 公共 origin。开发时可以复制示例并将两个受管目录改到仅当前用户可访问的绝对路径：

```bash
cp packaging/solodock.toml.example /tmp/solodock.toml
chmod 600 /tmp/solodock.toml
# 编辑 state_directory、runtime_directory 和 public_origin
SOLODOCK_CONFIG_PATH=/tmp/solodock.toml cargo run
```

服务启动时会创建权限为 `0700` 的 state/runtime 目录。首次启动时，`<runtime_directory>/bootstrap.token` 包含一次性初始化 token；token 不会输出到日志。把 token 和密码分别读入 shell 变量后，可以避免把真实值写进命令历史：

```bash
read -r SOLODOCK_BOOTSTRAP_TOKEN < /path/to/runtime/bootstrap.token
read -rs SOLODOCK_ADMIN_PASSWORD
export SOLODOCK_BOOTSTRAP_TOKEN SOLODOCK_ADMIN_PASSWORD
jq -n '{bootstrap_token:env.SOLODOCK_BOOTSTRAP_TOKEN,password:env.SOLODOCK_ADMIN_PASSWORD}' |
  curl --request POST https://solodock.example.com/api/v1/auth/bootstrap \
    --header 'Content-Type: application/json' \
    --data-binary @-
unset SOLODOCK_BOOTSTRAP_TOKEN SOLODOCK_ADMIN_PASSWORD
```

登录必须携带与 `public_origin` 精确匹配的 `Origin`，响应设置 Secure session 和 CSRF cookie。后续认证 mutation 还必须把 CSRF cookie 值放入 `X-CSRF-Token`。认证 API 不使用 `Idempotency-Key`。以下示例同样从交互式输入读取密码，并把 cookie jar 创建为仅当前用户可读：

```bash
read -rs SOLODOCK_ADMIN_PASSWORD
export SOLODOCK_ADMIN_PASSWORD
SOLODOCK_COOKIE_JAR="$(mktemp)"
chmod 600 "$SOLODOCK_COOKIE_JAR"
jq -n '{username:"admin",password:env.SOLODOCK_ADMIN_PASSWORD}' |
  curl --request POST https://solodock.example.com/api/v1/auth/login \
    --header 'Origin: https://solodock.example.com' \
    --header 'Content-Type: application/json' \
    --cookie-jar "$SOLODOCK_COOKIE_JAR" \
    --data-binary @-
unset SOLODOCK_ADMIN_PASSWORD
# 使用结束后删除 "$SOLODOCK_COOKIE_JAR"
```

SQLite 保存管理员凭据、session、登录节流和真实审计历史；应用及 release 的权威事实保存在文件系统，`active` symlink 是 active release 的唯一事实源。只有 HTTP listen 前的启动恢复会清理 crash 遗留的临时目录和未引用 revision；运行期 catalog/reconciliation 扫描严格只读，不能与正在发布的 revision 竞争。SQLite 丢失后，启动扫描会重建应用查询索引，但不能恢复管理员、session 或审计历史，因此必须重新执行 bootstrap。损坏的 SQLite 不会被自动替换。

SoloDock 可以管理 write-only Registry credential，把 draft tag 解析为当前 Docker 平台的具体 manifest digest，并通过唯一后台 deployment 状态机完成 pull、force-recreate、健康门禁、active 原子切换和失败回滚。每个应用可配置 `1–600` 秒的停机宽限，默认 `10` 秒；该值固定进 release，服务提前退出时部署立即继续。管理员可显式确认开启有界 Registry 轮询自动部署；busy 不排队，坏 target 会被抑制。所有持久业务 mutation 都要求 `Idempotency-Key`，并继续要求精确 Origin、session 与 double-submit CSRF。

控制台状态条同时展示主机 `MemAvailable` 与状态盘可用空间。系统设置可从后端认可的 IANA 时区下拉列表中选择全局显示时区，默认 UTC，保存后立即刷新 Web 时间；SQLite、API、SSE、cursor 与下载日志始终保留 UTC。应用配置页使用单列直接编辑表单，public 与 write-only Secret 环境变量统一逐行管理；部署历史固定一项一行。

新服务默认加入 SoloDock 管理的内部服务发现网络，服务间可使用 `<slug>:<container-port>` 互通且无需发布数据库宿主端口；旧 release 不会因升级被自动加入。内置“快速部署 → PostgreSQL”会生成普通 immutable draft，默认使用 `postgres:18`、owned data volume、自动生成的 write-only 密码和内部网络，然后通过正常 deployment 状态机部署；它不引入任意 Compose 或远程模板执行。

Registry credential 位于 `state/registry-credentials/<uuid>/`，metadata 与 immutable secret revision 分离；API 只返回 registry、username、revision 和时间，不回显 token。Docker pull 使用 `/run/solodock/docker-config/<deployment-id>/config.json` 的 operation-scoped 私有配置，并且命令参数只包含 digest-pinned image reference。启动会精确清理遗留 runtime credential 目录，并将 SQLite 中的 queued/running deployment 标记为 interrupted。

可选的签名 Registry recheck webhook 使用独立 `webhook_public_origin`、每应用 write-only HMAC secret、5 分钟 timestamp window 和 durable nonce 防重放。Payload 不携带镜像事实；有效请求只产生 coalesced poll wake，并继续遵守原有自动部署、退避、抑制、drift 和健康门禁。协议与外部 WAF 边界见 [Webhook 运维说明](docs/webhooks.md)。

Deploy/rollback 均返回 `202 Accepted` 和 deployment ID。可在应用页查看 transition timeline；`pending` 表示有尚未 commit 的 candidate 或需要人工重新收敛的中断现场。健康通过前 `active` symlink 不改变。回滚会恢复旧 release 的镜像和 Compose 配置，但不会回退数据库 migration、named volume 或 bind 内容。

每次 draft 更新先完整写入新的 `config-revisions/<uuid>/`，再以 `app.toml` 的原子替换作为 commit point，因此 draft 编辑不会改变正在运行的旧 release 所挂载的内容。生成的 Compose 固定为单一 `app` service，并由 `/usr/bin/docker compose` 的固定参数向量校验/执行；不存在 shell、exec、原始 Compose 编辑器或用户参数。

显式挂入容器的 public/secret managed file leaf 固定为 `0444 solodock:solodock`，支持镜像内任意常见非 root UID/GID 读取；宿主 state、应用与 revision ancestor 仍为 `0700`，Compose mount 继续 `read_only: true`。启动会在 listen 前把 canonical legacy `0400/0600` leaf 收敛到 `0444`，运行期权限漂移则由 strict loader 在 Compose effect 前 fail closed。

host bind 默认禁用。管理员在“系统设置 → 存储访问”逐行维护允许根目录；授权根必须是既有、无 symlink 的安全绝对目录，应用只能使用其严格子目录，并且授权根不得与 Docker daemon 报告的实际 data-root 重叠。正在被 draft/active/pending revision 引用的 root 不能删除。SoloDock 不浏览、创建、改权限、写入、备份或删除 bind source。每次 Compose mutation 都从 filesystem active release 重建并校验 canonical artifact，同时枚举 project/service 的全部候选并对 unmanaged、stale 或 invalid collision fail closed。unregister 默认不移除容器；即使显式移除精确 owned container，也保留 owned/external volume、bind 内容和 network。删除预览同样从 filesystem 生成，稳定合并 active pinned config 与当前 draft 的资源，并标明资源实际存在或仅配置；DELETE 在消费 token 和提交 tombstone 前都重算完整 facts hash。

```toml
allowed_bind_roots = ["/srv/solodock-data"]
```

上面的 TOML 字段只用于升级后的首次 SQLite bootstrap import；导入后以 Web/SQLite 全局设置为唯一事实源，后续重启不会重新合并 TOML。

运行后端验证：

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features -- --test-threads=2
cargo build --release --locked --features embed-ui
```

## 前端开发

```bash
cd web
npm ci
npm run dev
```

Vite 将 `/api` 和 `/healthz` same-origin proxy 到 `SOLODOCK_API_ORIGIN`（默认 `http://127.0.0.1:8080`）。浏览器仍应通过外部 tunnel 或 reverse proxy 提供的 HTTPS hostname 访问 Vite/proxy，Secure cookie 没有开发降级开关。生产构建把 Vite 产物嵌入单一 Rust binary，不运行 Node 服务。

运行前端验证：

```bash
npm run check
npm run test
npm run build
```

Node/npm 只用于开发和构建 Web UI。

## 文档

- 产品与配置：[产品范围](docs/product-scope.md)、[应用模型](docs/application-model.md)；
- 系统与发布：[架构](docs/architecture.md)、[部署与回滚](docs/deployments.md)、[API 与实时流](docs/api-and-streams.md)；
- 生产运行：[运维](docs/operations.md)、[恢复](docs/recovery.md)、[威胁模型](docs/threat-model.md)、[资源预算](docs/resource-budget.md)；
- 专题与验收：[Webhook](docs/webhooks.md)、[测试与安全护栏](docs/testing.md)。

这些专题文档描述当前实现。历史设计与交付计划由 Git 保留，不作为当前行为的事实来源。

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
