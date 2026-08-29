# SoloDock MVP 设计提案

- 状态：已确认
- 日期：2026-08-28
- 目标环境：Ubuntu 24.04、单机、单管理员
- 仓库：`baixiaohang/solodock`

## 1. 摘要

SoloDock 是面向个人单机 Docker 工作负载的轻量级部署控制台。它部署预构建容器镜像，提供精简的 Web UI，将可变镜像 tag 解析为不可变 digest，检查应用健康状态，并在新版本失败时回滚。

SoloDock 不是通用 Docker 管理套件，也不是完整 PaaS。它不构建源码、不管理域名或 TLS、不提供反向代理，也不编排多台主机。预期部署方式是在现有 Cloudflare Tunnel 和 IP 白名单之后，服务自身只监听宿主机 loopback。

MVP 有意采用严格受限的应用模型：

```text
一个 SoloDock 应用 = 一个 Docker service/container = 一个预构建镜像
```

SoloDock 为每个应用生成并管理最小 Compose 文件。用户通过 UI 配置受支持的字段；MVP 不导入任意已有 Compose project，也不提供原始 Compose 编辑器。

## 2. 背景与产品对比

目标主机是一台 2 vCPU、4 GiB 内存的 Ubuntu 24.04 腾讯云服务器，已经运行 Docker Compose、Cloudflare Tunnel、SoloGrove、PostgreSQL 和其他应用。控制面必须把绝大多数资源留给这些工作负载。

| 产品 | 优势 | 为什么不是本项目的目标 |
| --- | --- | --- |
| CapRover | 完整的部署体验，支持镜像/源码发布、健康检查、代理、TLS 和集群 | Docker Swarm、Nginx、Let's Encrypt、源码构建和集群能力超出需求；其 Compose 支持也是受限子集。 |
| Dockge | 轻量、文件导向的 Compose UI，包含生命周期控制和日志 | 核心侧重手动 Compose 管理，不覆盖不可变 release、自动 digest 发现、健康门禁发布历史和回滚闭环。 |
| Portainer | 覆盖 Docker、Swarm、Kubernetes、Registry 和基础设施管理 | 范围远大于应用发布；部分 GitOps/Webhook 能力还取决于产品版本。 |
| Dokku | 成熟的 Heroku 风格 CLI 部署与健康检查模型 | 以 Git/源码构建、插件、代理和 CLI 工作流为中心，不是小型 image-only Web 控制台。 |
| Coolify | 完整自托管 PaaS，包含 Git 集成、构建、代理、服务和回滚 | 控制面和本地构建范围对繁忙的 2C4G 主机过重；官方建议从 2 核 2 GiB 起步。 |
| Dokploy | 完整部署平台，包含 Compose/Swarm、Provider、构建、Traefik、监控和备份 | 包含大量 SoloDock 明确排除的能力。 |

参考资料：

- [CapRover](https://caprover.com/)
- [CapRover Docker Compose 支持](https://caprover.com/docs/docker-compose)
- [Dockge](https://github.com/louislam/dockge)
- [Portainer Webhook](https://docs.portainer.io/user/kubernetes/applications/webhooks)
- [Dokku 架构](https://dokku.com/docs/development/architecture/)
- [Coolify 安装要求](https://coolify.io/docs/get-started/installation)
- [Coolify Docker Compose 模型](https://coolify.io/docs/knowledge-base/docker/compose)
- [Dokploy Docker Compose](https://docs.dokploy.com/docs/core/docker-compose)

## 3. 目标

MVP 必须：

- 以低开销 Rust 服务运行；
- 支持单机和单管理员；
- 提供 Web 管理界面；
- 创建和管理多个单 service 应用；
- 接受来自 GHCR、Docker Hub 和兼容 Registry 的预构建 OCI/Docker 镜像引用；
- 支持私有 GHCR 拉取凭据；
- 为每个应用配置环境变量、挂载配置文件、端口、named volume、受限宿主路径 bind mount、network 和一套容器健康策略；
- 启动、停止、重启、部署、取消注册和移除应用容器，同时不删除 volume 或 bind mount 数据；
- 展示容器状态、有界日志流和实时 CPU/内存/网络统计；
- 轮询配置的镜像 tag，并将其解析为不可变 digest；
- 只按不可变 digest 部署；
- 等待健康策略达标，并在正常部署失败后自动恢复上一 digest；
- 保存部署历史并支持手动回滚；
- 禁止同一应用并发变更；
- 确保主机或进程中断后的部署可重试；
- 让应用配置和 release snapshot 能从文件恢复；
- 只监听 `127.0.0.1`，公网访问由外部 Cloudflare Tunnel 提供。

## 4. 非目标

MVP 不做：

- 管理 Nginx、Traefik、域名、DNS、TLS 或 Cloudflare 配置；
- 支持 Docker Swarm、Kubernetes、多主机、高可用、多租户或 RBAC；
- clone 仓库、构建 Dockerfile、运行 buildpack 或执行 `docker compose build`；
- 接受 GitHub 源码仓库 URL 作为部署输入；仓库必须先发布容器镜像；
- 在一个 SoloDock 应用内支持多 service 或多副本；
- 导入、扫描或接管任意已有 Compose 目录；
- 暴露任意 Compose YAML 编辑器；
- 提供浏览器 shell、宿主命令执行器、容器 exec 终端或自定义 Compose 参数；
- 接受未由宿主配置预先授权的任意 bind mount 路径，或挂载宿主根目录、Docker socket 和其他敏感路径；
- 自动修改、备份、恢复、prune 或删除已有 named volume，或创建、修改权限、备份、恢复、prune 或删除 bind mount 源目录；
- 承诺零停机部署；
- 回滚数据库 schema 或持久化数据；
- 在断电后自动精确续跑到部署中断的 phase；
- 在 MVP 中验证 Cosign/Sigstore 签名。

## 5. 应用与配置模型

### 5.1 镜像输入

每个应用只接受一个镜像引用，例如：

```text
ghcr.io/baixiaohang/sologrove:staging
ghcr.io/example/private-api:v1
postgres:16-alpine
docker.io/dpage/pgadmin4:latest
```

可变 tag 只用于发现新版本。release 始终记录并运行以下形式的引用：

```text
registry.example/namespace/image@sha256:<digest>
```

对于多平台镜像，SoloDock 同时记录 Registry index/manifest digest、选中的 platform 和本地 Docker image ID，使 release 始终可解释。

### 5.2 环境变量

UI 对同一份规范环境变量数据提供两种视图：

- 表格视图：逐行编辑 key/value，并显式标记 secret；
- 批量视图：解析并校验标准 `.env` 输入，拒绝重复或非法 key，保存前显示 diff。

两种视图不得分别保存数据。非 secret 值可在两种视图间完整往返转换。API 只接受 secret 写入，永不返回原值。掩码占位符不得覆盖已保存 secret；批量更新 secret 时必须明确选择保留、替换或删除。

生成的 Compose 文件只包含变量引用，不包含 secret 值。公开值与 secret 值保存在不同的权限受限文件中。

### 5.3 挂载配置文件

应用可以管理有大小上限的文本配置文件，例如 JSON、YAML、dotenv、证书或 PEM。每项配置包含：

- 逻辑名称；
- 容器内目标路径；
- 是否敏感；
- 是否只读挂载；MVP 强制只读；
- 单文件大小和应用总配额。

普通文件和 secret 文件使用不同的宿主目录与权限。secret 内容首次提交后只能通过 API 替换，不能读取。路径必须规范化，不得指向 Docker socket、宿主根目录或其他敏感宿主路径。

### 5.4 端口、volume、bind mount 与 network

- 发布端口默认显式绑定 loopback，例如 `127.0.0.1:8000:8000`。
- MVP 拒绝绑定非 loopback 宿主地址。
- SoloDock 可以创建应用自有 named volume，也可以挂载显式指定的已有 named volume。
- 已有 volume 视为 external，永不修改或删除。
- 移除应用绝不向 Compose 传入 `-v`/`--volumes`。
- start、stop、restart、deploy、rollback、unregister、容器移除和应用删除都只能改变容器及其挂载关系，不得清空、覆盖、迁移或删除 named volume 与 bind mount 源目录中的实际内容。
- 宿主配置通过 `allowed_bind_roots` 声明可以用于持久数据的目录根，例如 `/srv/solodock-data`；默认列表为空，即禁用宿主路径 bind mount。
- 应用只能选择某个授权根目录下已经存在的子目录，并配置绝对的容器内目标路径。授权根、源目录和目标路径都必须规范化；源目录必须在每次 validate、preview 和 Compose mutation 前重新解析，并保持在授权根内。
- 授权根和源目录不得是 symlink；包含 symlink、`..`、路径逃逸、Docker socket、宿主根目录或其他敏感宿主路径的输入一律拒绝。应用不得直接挂载整个授权根。
- bind mount 默认只读；可读写挂载必须由用户显式选择并确认无法随 release 回滚的警告。配置文件继续使用 5.3 节的受管只读机制，不借用通用 bind mount 绕过 secret 和配额约束。
- SoloDock 不创建 bind mount 源目录，不执行 `chown`/`chmod`，不修改、备份或删除其中内容。unregister、容器移除和应用删除都保留源目录及其数据。
- named volume、bind mount 和受管配置文件的容器目标路径不得冲突。validate、deploy preview 和 deletion preview 必须展示规范化后的宿主源路径、容器目标路径和读写模式。
- 每个应用拥有隔离的默认 network，也可挂载显式指定的已有 external network，以连接共享 PostgreSQL 等依赖。
- 移除应用绝不删除 external network。

### 5.5 健康策略

每个应用选择以下一种策略：

- `healthy`：镜像或生成的 Compose service 必须定义 Docker healthcheck，并达到 `healthy`；
- `running`：容器必须在稳定窗口内持续运行，默认 15 秒；
- `completed`：仅用于显式配置的一次性应用，要求退出码为 0；
- `disabled`：只允许在明显警告后选择；自动回滚只能检测启动失败。

UI 可以提供 HTTP 健康检查表单并生成 Docker healthcheck，但必须说明相关命令需要存在于镜像内。默认优先采用镜像内置 healthcheck，除非用户明确覆盖。

## 6. 架构

```text
Browser
  -> fixed VPN/TUN egress IP
  -> Cloudflare WAF allowlist
  -> Cloudflare Tunnel
  -> 127.0.0.1:<port>
  -> SoloDock (Rust)
       |-- REST + SSE API
       |-- filesystem application/release store
       |-- SQLite operational ledger
       |-- Docker Engine API via Bollard (observation)
       |-- docker compose CLI (exact application mutations)
       `-- OCI Registry V2 / GHCR digest resolver
```

SoloDock 是单个 Rust 进程和单个 Rust crate，不引入内部服务、插件系统或通用 workflow engine。

### 6.1 技术选型

- Rust stable，edition 2024。
- Axum + Tokio：HTTP、中间件、流式传输和任务协调。
- MVP 使用 Server-Sent Events 而不是 WebSocket，因为状态、日志、统计和部署进度都是服务端到客户端的单向流。SSE 用更少的协议状态提供重连语义。
- Bollard：Docker list、inspect、events、stats、logs 和 image inspect。
- 官方 Docker Compose CLI：校验生成的 project 并执行生命周期变更。SoloDock 永不调用 shell，只构造固定参数向量。
- SQLx + SQLite WAL：session、部署任务、幂等记录、审计事件和查询索引。
- Serde/TOML：自有配置。YAML 生成仅覆盖 SoloDock 的小型 Compose schema，官方 Compose 校验仍是权威。
- 基于 Reqwest 的 OCI Distribution adapter：解析 manifest digest 和 bearer-token 认证，首批覆盖 GHCR 与 Docker Hub。
- Svelte + TypeScript + Vite：构建静态前端；最终发布版本将产物嵌入 Rust 二进制。
- Tracing：结构化日志；Argon2id：管理员密码；secrecy/zeroize 类封装：敏感值。

### 6.2 事实来源边界

每类事实只有一个权威来源：

- 应用配置、公开环境数据、挂载文件、credential 引用和不可变 release snapshot：文件系统；
- secret 值：专用权限受限文件，永不复制进 Compose 或 SQLite；
- 容器实际状态：Docker daemon；
- 管理员凭据、session、认证节流、部署执行状态、幂等键、审计事件和可重建查询索引：SQLite；
- 可变 tag 当前 digest：Registry；release 创建后，该 release 中记录的 digest 即为权威。

`active` symlink 是 active release 的唯一权威来源；应用配置文件不重复保存 active release ID。文件系统权威事实先原子提交，再刷新 SQLite 查询投影，不虚构跨存储事务。即使 SQLite 丢失，系统仍必须能扫描应用目录，恢复应用、active release、生成的 Compose 和镜像 digest。数据库丢失后不得重建或伪造管理员、session 或历史审计记录，管理员需要重新 bootstrap。

关键文件写入使用同目录临时文件、文件 `fsync`、原子 rename 和父目录 `fsync`。release 和 config revision 目录创建后保持不可变。draft 更新先发布完整的 `config-revisions/<revision-id>/`，最后原子替换 `app.toml` 使 `draft_revision` 指向新 revision；这一步是 commit point。每个 release 固定引用自己的 config revision，后续 draft 编辑不会改变 active 容器的挂载内容。

## 7. 宿主存储布局

```text
/etc/solodock/config.toml

/var/lib/solodock/
  state.sqlite3
  registry-credentials/<credential-id>/
    credential.toml
    secret-revisions/<revision-id>/token
  registry-credentials/.trash/<credential-id>-<operation-id>/
  apps/<app-id>/
    app.toml
    config-revisions/<revision-id>/
      config.toml
      env/public.env
      secrets/runtime.env
      files/public/<name>
      files/secret/<name>
    releases/<release-id>/
      release.toml
      compose.yaml
    active -> releases/<release-id>
    pending -> releases/<release-id>          # 仅在部署中或等待重试时存在

/run/solodock/
  locks/<app-id>.lock
  compose/<operation-id>/compose.yaml
  docker-config/<operation-id>/config.json
```

启动时强制检查预期文件权限。Registry 认证在一次操作范围内写入 `/run` 下的临时 `DOCKER_CONFIG` 目录，永不通过命令参数传递，并在操作结束后删除。

## 8. 核心数据模型

### App

```text
id, slug, display_name, project_name
discovery_image_ref, credential_ref
desired_state, auto_deploy_enabled, poll_interval
ports[], named_volumes[], bind_mounts[], networks[]
health_policy
schema_version, created_at, updated_at
```

`project_name` 只生成一次且不可变，每次 Compose 命令都显式传入。用户提供的 slug 未经校验时绝不直接作为 CLI 参数。

### Release

```text
id, app_id, config_sha256
source_image_ref, resolved_digest, platform, local_image_id
compose_snapshot_path
trigger_metadata, created_at
```

release 创建后不可变。手动回滚会创建一个以旧 release 为 candidate 的新 deployment，绝不修改历史记录。

### Deployment

```text
id, app_id
trigger: manual | poll | rollback | config
from_release_id, candidate_release_id
status, phase, idempotency_key
error_class, error_code, redacted_message
started_at, completed_at
```

### RegistryCredential

```text
id, registry, username, metadata_revision, secret_revision
last_operation_id, integrity_hmac, created_at, rotated_at
```

API 只返回 credential metadata。GHCR 文档应建议在可行时使用专用 classic PAT，且仅授予 `read:packages`。

credential metadata 与 immutable secret revision 都由 filesystem 作为事实源；HMAC 同时覆盖 metadata 与 token hash。Registry host 创建后不可修改，轮换只允许修改 username 并显式 keep/replace secret。任何 draft、active/pending 或历史 v2 release 引用存在时，删除返回 `CREDENTIAL_IN_USE`。

### AuditEvent

```text
id, actor, request_id, action
target_type, target_id, result
redacted_metadata, created_at
```

容器状态和 metrics 从 Docker 动态派生，不作为竞争性的持久业务状态。

## 9. 部署状态机

```text
QUEUED
  -> RESOLVING      tag -> digest；相同 digest 记录为 no-op
  -> PREPARING      校验配置并原子写入 candidate release
  -> PULLING        只拉取 image@digest
  -> APPLYING       使用不可变 candidate 的生成 Compose
  -> VERIFYING      等待健康策略与稳定窗口达标
  -> COMMITTING     原子切换 active release，并提交 DB 状态
  -> SUCCEEDED

APPLYING 前失败 -> FAILED，运行现场不变
APPLYING 后正常失败 -> ROLLING_BACK -> VERIFYING_ROLLBACK
                                      |-> ROLLED_BACK
                                      `-> NEEDS_ATTENTION

宿主/进程中断 -> INTERRUPTED；若 actual != active，再标记 DRIFTED
下一次手动或轮询部署 -> 从头执行目标 candidate
```

### 9.1 并发

- SQLite 原子 claim 应用变更；应用级 owned mutex 与 advisory file lock 提供第二层进程边界。deployment schedule 在同一短事务持久 idempotency claim、queued row、transition 和 audit attempt。
- 并发变更返回 `409 APP_BUSY`。
- 全局部署 semaphore 默认值为 1，限制 2C4G 主机上的镜像拉取和解压压力。
- 轮询不建立无限队列。应用繁忙时留到下一轮检查，自然合并中间 tag 变化。

### 9.2 中断模型

MVP 不自动推断并精确续跑中断的 phase。

- Docker 变更前，candidate release 已持久化；
- 验证成功前，active release 保持不变；
- 启动时，所有非终态任务标记为 `INTERRUPTED`；
- SoloDock 比较 active 期望 digest 与容器实际 digest，并展示 drift；
- drift 未解决时，阻止 start、restart 和配置变更；
- 下一次手动 Deploy 或 Registry poll 从头执行目标 candidate。Compose 操作必须幂等，使单容器收敛到该 release；
- 若重试 candidate 正常失败，SoloDock 恢复并验证 active release。

宿主重启后，已有容器由 Docker 配置的 restart policy 负责拉起。SoloDock 启动时不进行推测性清理。

### 9.3 回滚边界

回滚只恢复生成的 Compose 配置和不可变镜像 digest，无法撤销数据库 migration、bind mount 写入或 named volume 内容。存在不可逆 migration 的应用必须采用向后兼容的 migration 模式，或在明确警告后关闭自动回滚。

## 10. Registry 轮询与可选 Webhook

Registry 轮询是自动部署的主要机制：

1. 使用所需 OCI/Docker media type，通过 Registry manifest endpoint 解析配置的 tag；
2. 完成 bearer-token 认证且不泄露 credential；
3. 将返回 digest 与 active release digest 比较；
4. 若 digest 不同且应用空闲，按 digest 发起部署；
5. 对 Registry/瞬时错误使用 jitter 和有界指数退避。

默认轮询间隔为五分钟，可在安全范围内按应用配置。

签名部署 Webhook 延后到核心 MVP 之后。如果实现，必须使用独立 hostname、精确 WAF path/method 规则、HMAC-SHA256、timestamp 窗口、nonce 防重放和 body 大小限制。Webhook 只提示系统重新查询 Registry；请求内容绝不能直接成为可信 Docker 镜像参数。

## 11. 认证与威胁模型

### 11.1 部署边界

推荐的生产访问链复用 SoloGrove 现有 staging 方式：

```text
Browser -> fixed VPN/TUN egress IP -> Cloudflare WAF allowlist
        -> Cloudflare Tunnel -> 127.0.0.1:<SoloDock port>
```

- 腾讯云安全组和宿主防火墙均不开放 SoloDock HTTP/HTTPS 端口，包括 IPv4 和 IPv6；
- `cloudflared` 是唯一公网入口，并通过出站连接建立 Tunnel；
- Cloudflare WAF 是第一层过滤，但不能替代应用认证；
- SoloDock 仍必须要求管理员密码；
- 对固定 IP、单管理员 MVP，Cloudflare Access/MFA 为可选；若取消 IP 白名单、需要移动网络访问或增加管理员，则建议启用。

### 11.2 应用认证

- 只有一个管理员账号；
- 初始密码通过一次性 loopback bootstrap token 设置，不能由第一个公网访问者注册；
- 强唯一密码使用 Argon2id 哈希；
- session cookie 设置 Secure、HttpOnly、SameSite=Strict；
- mutation 使用 CSRF token 和精确 Origin 校验；
- 登录限速/冷却、有限 session 生命周期和撤销全部 session；
- 审计登录成功、失败和敏感操作，但不记录 secret 或 cookie。

不使用 HTTP Basic Authentication。

### 11.3 Docker socket 边界

SoloDock 使用专用、禁止登录的 system user 运行，不使用 UID 0：

```ini
User=solodock
Group=solodock
SupplementaryGroups=docker
```

但是，加入 Docker group 并访问 `/var/run/docker.sock` 在效果上等同 root 权限。被攻陷的 SoloDock 进程可以要求 Docker 挂载宿主根目录或启动 privileged workload。非 root UID 能减少普通文件权限错误，但不能成为抵御 Docker daemon 权限的安全边界。

缓解措施：

- 绝不通过 Web API 暴露 Docker socket，也不把 socket 提供给受管应用容器；
- 绝不启用未认证的 Docker TCP；
- 只提供固定生命周期动作和结构化配置字段；
- 在破坏性操作前核对精确 Compose project name、Docker label 和实际对象 ID；
- 绝不调用 shell，也不接受任意命令参数；
- 宿主 bind mount 只允许来自管理员配置的根目录，默认禁用；每次 mutation 前重新校验规范路径、symlink 和敏感路径，并默认只读；
- 应用和 Registry secret 不得进入日志、错误、审计 metadata、Compose 文件、进程参数或普通 API 响应；
- 限制日志行长度、stream buffer、速率和并发连接数；
- 对 root 用户、privileged mode、host namespace、device 或 Docker socket mount 做 lint 和醒目警告。MVP 的结构化模型应拒绝所有不支持的能力。

宿主 root 已失陷、Docker daemon 已失陷以及恶意唯一管理员，不属于同机控制面能够防护的范围。

### 11.4 M2 只读观察边界

- 生产代码只连接固定 `/var/run/docker.sock`，不读取 `DOCKER_HOST`，不接受自定义 socket、TCP endpoint 或 TLS credential；
- Docker socket 缺失、权限不足、daemon 重启或 API 不兼容不会阻止认证控制面启动。system health 和 app catalog 继续可用并明确 degraded，stream 在发送 headers 前返回稳定 `503`；
- 一个容器只有同时精确匹配 `com.solodock.managed=true`、`com.solodock.schema-version=1`、canonical app/release UUID、Compose project、`service=app` 和 `oneoff=False`，且 app 存在于 filesystem catalog 时才属于 SoloDock；
- list、detail、drift、events、logs 和 stats 共用同一 ownership validator，stream 建立前按 full container ID 重新 inspect；非 SoloDock 容器完全忽略；
- API 只返回 SoloDock 自有 allowlist DTO，不序列化 Bollard raw model、环境变量、命令、任意 labels、HostConfig 或 daemon 原始错误；
- M2 生产代码只调用 Docker ping/version/info/list/inspect/events/logs/stats，不提供 create/start/stop/restart/remove、Compose、exec/shell 或应用 CRUD。

M2 的 SSE 固定全局上限 24、单 session 8；events 全局 16/单 app 4，logs 和 stats 各全局 8/单 app 2。events/logs queue 上限 128，慢消费者收到 `SLOW_CONSUMER` 后断开；stats 只保留最新 sample，最后一个 subscriber 离开后取消采样。15 秒 heartbeat 重新验证 session，revoke-all 和过期 session 最迟在一个 heartbeat 周期内关闭连接。events 使用每进程 boot UUID + 单调序号和 512 条 ring 支持 replay/reset；logs 使用 timestamp cursor，保持 at-least-once 边界语义。

日志先按完整 logical line framing，再以 byte 形式脱敏 SoloDock 已知的受管 secret；跨 Docker chunk 的 secret 仍会替换为 `[REDACTED]`。64 KiB 以上原始行整行省略，正常 message 上限 16 KiB，并移除 NUL 和终端控制序列。应用自行产生、SoloDock 从未持有的 secret 无法可靠识别，系统不对此作虚假保证。

### 11.5 M3 mutation 与 Compose 边界

- `POST /api/v1/apps` 只登记应用并发布 immutable draft revision；不解析 tag、不 pull、不创建 container，也不暗中执行 M4 deploy；
- 所有持久业务 mutation 使用 16–128 字节安全 ASCII `Idempotency-Key`。SQLite 只保存 key/request 的 HMAC、operation ID、状态和脱敏响应；同 key 不同请求返回冲突；
- Compose production runner 固定执行 `/usr/bin/docker`，清空继承环境，不读取 `DOCKER_HOST`/context，不使用 shell，显式禁用项目 `.env`，只能产生 validate/start/up/stop/restart/rm 的封闭参数向量；
- 每次 runtime 操作都从 filesystem `active` symlink 重读 digest-pinned image、release ID 与 verified pinned config revision，重建 canonical Compose，并与持久 `compose.yaml` 逐字校验后才执行。紧邻 CLI spawn 前按 Compose project/service 枚举全部 container；任一 unmanaged、invalid、stale-release 或多候选 collision 都 fail closed。缺少 config pin 或 artifact 不一致时同样 fail closed，不执行 legacy artifact；
- owned volume/network 与 container 在操作前按精确 ownership 重新 inspect；external resource 必须存在。所有 bind source 在 spawn 前重新验证 allowlist、symlink、device/inode；
- unregister 默认保留 container。显式 remove 只执行 `compose rm --stop --force app`，永不执行 `down`、`-v/--volumes` 或 `--remove-orphans`，所有 volume、bind 数据和 network 均保留；
- Docker capability ready 后，`allowed_bind_roots` 与每个 bind source 还必须和 daemon 报告的实际 Docker data-root 做双向 overlap 检查；非默认 data-root 与运行期中间目录 symlink swap 均 fail closed；
- create/update/delete 先提交 filesystem 权威事实，再整快照替换内存 catalog 并刷新 SQLite 投影。后续投影失败产生稳定 warning 和 degraded health，由受 shutdown 管理的 reconciliation worker 重试；不得把已提交事实误报为回滚。
- crash 遗留 temp 与未引用 revision 只允许在 HTTP listen 前的 startup recovery 清理；运行期 verified-active、catalog refresh 与 projection reconciliation 使用只读扫描，不能删除并发 writer 正在发布的 artifact。
- 删除 preview 与 DELETE 在同一 catalog→app 协调锁序内从 filesystem、verified active config 和 exact Docker observation 派生 canonical facts。token 绑定 active+draft 的去重资源清单、实际存在/仅配置 disposition 与完整 facts hash，并在 consume 和 tombstone 前重验；删除 producer barrier 在 tombstone 前失败时必须恢复新的 stream cancellation generation。

## 12. API 草图

所有持久业务 mutation endpoint 接受 `Idempotency-Key`。认证协议 endpoint 不接受该 header：login 会生成随机 session 和 cookie，bootstrap 已由 singleton credential 与一次性 token 保证至多一次。错误使用统一格式：

```json
{
  "code": "APP_BUSY",
  "message": "The application already has an active mutation",
  "request_id": "..."
}
```

错误详情绝不包含 secret。

```text
POST   /api/v1/auth/bootstrap
POST   /api/v1/auth/login
POST   /api/v1/auth/logout
GET    /api/v1/me
POST   /api/v1/me/sessions/revoke-all

GET    /api/v1/apps
POST   /api/v1/apps
GET    /api/v1/apps/{id}
PUT    /api/v1/apps/{id}/draft
POST   /api/v1/apps/{id}/validate
POST   /api/v1/apps/{id}/deployments
POST   /api/v1/apps/{id}/actions/start
POST   /api/v1/apps/{id}/actions/stop
POST   /api/v1/apps/{id}/actions/restart
POST   /api/v1/apps/{id}/deletion-preview
DELETE /api/v1/apps/{id}

GET    /api/v1/apps/{id}/deployments
GET    /api/v1/deployments/{id}
POST   /api/v1/deployments/{id}/rollback

GET    /api/v1/apps/{id}/events
GET    /api/v1/apps/{id}/logs
GET    /api/v1/apps/{id}/stats

GET    /api/v1/registry-credentials
POST   /api/v1/registry-credentials
PUT    /api/v1/registry-credentials/{id}
DELETE /api/v1/registry-credentials/{id}

GET    /api/v1/system/health
GET    /api/v1/system/drift
```

events、logs 和 stats endpoint 都是有界 SSE stream。日志只接受与 service 无关的有限筛选项，例如 tail 数量和 since 时间；不存在 shell 或 exec endpoint。

M2 的所有 `/api/v1/**` read endpoint 都要求有效 session，并设置 `Cache-Control: no-store`。`GET /api/v1/system/health` 在认证后始终返回 `200`，将 Docker、filesystem recovery、state/Docker disk 和 active stream 数分别投影；`GET /api/v1/apps` 与 detail 在 Docker 不可用时仍返回 filesystem app 和 typed drift；`GET /api/v1/system/drift` 在无法完整观察时返回 `complete=false`，不会把未知状态误报为 container missing。

破坏性删除采用两阶段操作。preview 从 filesystem 权威事实返回精确 project、container、active/draft config、network、自有文件、保留 volume 和保留 bind mount 源目录，资源稳定去重并标明来自 active、draft 或两者以及实际存在/仅配置，随后附带短期 confirmation token。删除请求同时提交 token 和应用 slug，并在消费 token 与 tombstone 前重验完整 preview facts hash。默认仅 unregister；移除容器必须显式选择，且仍然保留所有 volume 和 bind mount 数据。

## 13. UI 草图

```text
+ Dashboard ------------------------------------------------+
| Docker OK | disk 61% | active deployment: none           |
| [New application]                                        |
|                                                          |
| SoloGrove  healthy  sha256:ab...  CPU 3%  RAM 420 MiB   |
| insight    running  sha256:cd...  CPU 1%  RAM 110 MiB   |
| pgAdmin    stopped  [Start]                              |
+----------------------------------------------------------+

Application / SoloGrove
  Overview | Configuration | Deployments | Logs | Settings
```

- Dashboard：Docker/system 健康、磁盘压力、部署活动和应用卡片；
- New application：镜像引用、credential、环境变量、文件、端口、named volume、授权根目录内的 bind mount、network、健康与自动部署策略；创建前进行校验和精确 preview；
- Overview：实际 digest 与 active digest、容器状态、端口、mount、network 和实时资源摘要；固定生命周期动作从 M3 开始提供，M2 UI 严格只读；
- Configuration：环境变量表格/批量编辑、write-only secret、挂载文件和 deploy 前 preview；
- Deployments：trigger、来源 tag、不可变 digest、phase、耗时、健康结果、错误分类和回滚关系；
- Logs：有界 tail/stream、暂停和下载当前窗口；无 terminal；
- Settings：轮询、Registry credential 引用、警告、unregister 和删除 preview。

## 14. 计划中的仓库结构

```text
Cargo.toml
Cargo.lock
rust-toolchain.toml
src/
  main.rs
  config.rs
  domain/
  api/
  auth/
  app_store/
  db/
  docker/
  compose/
  deploy/
  registry/
  security/
  telemetry/
web/
  package.json
  src/
migrations/
tests/
  fixtures/
  integration/
  e2e/
packaging/
  systemd/solodock.service
docs/
  proposals/
  operations.md
  recovery.md
.github/workflows/
```

在出现真正可独立复用或发布的边界前，项目保持单 Rust crate。

## 15. 交付计划

每项任务都必须可独立测试。具体实现路径可以细化，但不得改变上述边界。

### M0：仓库与可执行骨架

- 创建 Rust/Axum 与 Svelte/Vite 骨架、格式检查、lint、测试、CI、仓库元数据和开发 README；
- 增加默认 loopback 服务、`GET /healthz` 和 graceful shutdown；
- 增加前端 type check/build 脚本和最小页面；
- 增加使用专用 `solodock` 用户的 systemd 包装骨架。

路径：`Cargo.toml`、`src/`、`web/`、`.github/workflows/`、`packaging/`、根目录 metadata。

验证：Rust format/clippy/test、前端 check/build 和 CI 全部通过；服务按照 bootstrap 范围拒绝或忽略非 loopback 公网绑定。

### M1：持久存储与认证

- 实现宿主配置、原子应用/release 文件、SQLite migration、恢复扫描、结构化错误和 tracing；
- 实现一次性本地 bootstrap、login/session/CSRF、loopback 强制限制和登录审计。

路径：`src/config.rs`、`src/app_store/`、`src/db/`、`src/auth/`、`src/api/auth.rs`、`migrations/`。

测试：原子写恢复、权限强制、DB 索引重建、认证重放、CSRF、session 撤销和非 loopback 拒绝。

### M2：只读 Docker 控制台

- 通过 label 实现 Docker capability probe 和精确应用发现；
- 通过 Bollard 实现容器状态、有界日志、按需 stats 和 events；
- 实现 Dashboard、Overview 和 Logs 页面。
- 补齐 bootstrap/login/logout auth shell；M2 前端由 Vite same-origin proxy 提供，静态资源到 M5 才嵌入 Rust binary。

路径：`src/docker/`、`src/api/streams.rs`、`web/src/`。

测试：fake Docker 单测和隔离 daemon 集成测试；慢客户端、重连、速率/buffer 限制和 secret canary。

### M3：受管单 service 生命周期

- 实现结构化应用 schema 和最小生成 Compose adapter；
- 实现环境变量表格/批量解析、挂载普通/secret 文件、loopback 端口、named volume、授权根目录内的 bind mount、network 和健康策略；
- 实现校验/preview，以及精确 create/start/stop/restart/unregister/remove 动作。

路径：`src/domain/`、`src/compose/`、`src/security/`、`src/api/apps.rs`、配置 UI。

测试：注入与路径逃逸、bind mount allowlist/symlink/重复目标路径、只读默认和显式可读写确认、重复环境变量 key、write-only secret、external 资源、project 冲突、命令超时，以及全部生命周期动作绝不使用 `-v`、清空或删除 named/external/bind mount 数据的证明。

### M4：Digest release 与回滚

- 实现 GHCR/Docker Hub Registry digest 解析和 operation-scoped 临时 Docker 认证；
- 实现不可变 release、应用/全局锁、部署状态机、健康门禁、自动回滚、手动回滚和 interrupted/drift 检测；
- 实现部署历史和详情 UI。

路径：`src/registry/`、`src/deploy/`、`src/app_store/releases.rs`、部署 UI。

测试：public/private Registry、401/403、manifest list、tag race、精确 digest 部署、并发 `409`、正常失败的各 phase、健康失败回滚、回滚失败和中断后重试。

### M5：自动部署与生产硬化

- 实现带 jitter 的 Registry 轮询、no-op 和 coalescing 行为；
- 将静态前端资源嵌入 Rust 发布二进制；
- 完成 systemd 安装、升级、备份、恢复、威胁模型和运维文档；
- 在目标主机规格上测量 idle/stream/deploy 资源；
- 为 pgAdmin、insight-agent 和 SoloGrove 编写一次性迁移 runbook。这些是运维流程，不是通用 import 功能。

路径：`src/registry/poller.rs`、packaging、`docs/`、release workflow、E2E suite。

测试：轮询错误/退避、相同 digest no-op、繁忙应用合并、安装 smoke test、静态资源服务、资源预算和完整隔离部署/回滚流程。

## 16. 测试策略与安全

### 16.1 分层

- 纯单元测试通过 trait/fake 覆盖 Docker、Compose、Registry、clock 和文件系统边界；
- 集成测试在临时目录中覆盖 SQLite 和原子文件行为；
- CI Docker E2E 使用专用 Docker-in-Docker daemon，绝不把 CI 宿主 Docker socket 挂进测试控制面；
- 服务器验收测试必须显式指定测试 Docker context 或显式 opt-in，只创建带 run-token label 的随机 `solodock-test-<uuid>` project。

### 16.2 破坏性测试护栏

- cleanup 只使用本次测试运行记录的精确 ID；
- cleanup 前先验证 project prefix、测试 label 和 run token；
- 测试绝不运行 `docker system prune`、通配删除、全局镜像清理或 `compose down -v`；
- 测试绝不扫描并删除自己未创建的对象；
- bind mount 测试只使用本次运行创建的临时目录；cleanup 不删除目录内的模拟持久数据，除非它是测试框架在隔离临时根下记录的精确 fixture；
- 已有 SoloGrove、PostgreSQL、insight-agent、pgAdmin、network 和 volume 永不进入测试 selector。

### 16.3 MVP 验收标准

- tag 解析为 digest，运行容器使用 `image@sha256:...`；部署期间 tag 再变化也不影响 candidate；
- 同一应用的两个并发变更只产生一个 claim，另一个返回 `409 APP_BUSY`；
- 正常的不健康 candidate 会恢复并验证上一 digest，且不删除或替换 volume；
- 中断部署标记为 interrupted/drifted；下一次部署收敛到选定 release，最终成功或进入正常回滚路径；
- 环境变量表格与批量视图共享同一份规范值，重复 key 被拒绝；
- SoloDock 已知的受管 secret canary 不出现在普通 API 响应、SSE、审计行、tracing、错误、Compose 文件、release 文件或 CLI 参数中；应用自行产生且控制面从未持有的值不在此保证范围内；
- start、stop、restart、重新部署、回滚、unregister、容器移除和应用删除都保留全部 named/external volume、bind mount 实际内容和 external network；
- bind mount 源目录只有位于 `allowed_bind_roots` 下且不存在 symlink/路径逃逸时才能进入生成的 Compose；默认只读，可读写必须显式确认，应用删除后宿主目录及其数据保持不变；
- 删除演练副本中的 SQLite 后，仍可从文件系统恢复应用、active release、生成 Compose 和镜像 digest；
- 管理 HTTP 只绑定 loopback，且不存在 shell/exec endpoint；
- Docker E2E cleanup 无法匹配已有宿主应用或数据。

## 17. 资源预算与宿主运行方式

以下是 M5 需要验证的设计预算，不是对尚未实现二进制的实测承诺。

| 资源 | 目标预算 |
| --- | --- |
| Rust 控制面与嵌入 UI 的 idle RSS | 40–100 MiB |
| 空闲 CPU | 通常低于 1%，轮询/events 除外 |
| 活跃 UI stream | 在连接/buffer 硬限制内额外 10–40 MiB |
| Compose/pull 客户端瞬时内存 | 约 100–300 MiB；Docker daemon 解压可能更高 |
| SoloDock 二进制/UI/metadata | 二进制与资源数十 MiB；不含 Docker 镜像层的 metadata 目标低于 100 MiB |

运行默认值：

- 原生 systemd 服务，不再包装为另一个控制面容器；
- 专用、禁止登录的 `solodock` 用户，加入 Docker supplementary group；
- 只监听 `127.0.0.1`；
- `UMask=0077`、显式读写目录、失败重启、任务和文件描述符限制；
- 初始 `MemoryHigh` 约 256 MiB；只有实测后才设置硬限制，避免关键回滚被杀死；
- 全局部署并发为 1；
- Registry 默认每五分钟轮询并带 jitter；
- 只有 UI subscriber 存在时才采样 Docker stats；
- `allowed_bind_roots` 默认为空；只有管理员在宿主配置中显式加入根目录后才启用 bind mount；
- 报告并警告磁盘空间，但不自动 Docker prune。

在 2C4G 主机上，镜像拉取/解压和应用重启比 SoloDock 空闲进程更可能形成资源压力。因此，串行部署、无本地构建和部署前磁盘/内存检查是核心要求。

## 18. 主要风险

| 风险 | 影响 | 缓解 |
| --- | --- | --- |
| 单容器原地替换 | 短暂停机 | 明确为非目标；使用健康门禁和快速 digest 回滚。 |
| 不可逆应用 migration | 旧镜像可能无法使用变更后的数据 | 强警告、可关闭自动回滚、expand/contract migration 指南和独立备份。 |
| Docker socket 被攻陷 | 影响等同宿主 root | 受限且认证的 loopback API、固定动作、无 shell、精确目标、WAF/Tunnel/密码分层和安全测试。 |
| bind mount 暴露宿主数据 | 容器可能读取、修改或破坏宿主目录 | 默认禁用、宿主 allowlist、拒绝 symlink/路径逃逸与敏感路径、只读默认、读写显式确认，并且不自动管理目录内容。 |
| Compose CLI 版本差异 | mutation 行为或参数不一致 | 启动时 capability probe 并定义最低支持版本；不兼容时禁用 mutation，但保留只读观测。 |
| Registry 故障或限流 | 自动部署延迟 | 条件请求（可用时）、jitter、有界退避和明确错误分类；当前 release 不受影响。 |
| 多平台 manifest 混淆 | 审计或恢复信息错误 | 记录 index/manifest digest、platform 和 local image ID；测试 amd64/arm64 fixture。 |
| 第三方输出包含 secret | credential 泄露 | 不使用 argv、最小化捕获输出、统一脱敏、canary 测试和仅结构化错误摘要。 |
| SQLite 或磁盘损坏 | 执行历史或状态丢失 | WAL/checkpoint、备份文档、从文件恢复 app/release、磁盘预检；绝不伪造审计历史。 |
| 已有应用迁移出错 | 停机或挂载错误 volume | 应用专用维护 runbook、精确 preview、显式已有 volume/network 名称，且不做通用自动接管。 |

## 19. 架构原则检查

- **单一事实来源：** 文件负责应用/release 的期望事实，Docker 负责运行现场，SQLite 负责操作历史。环境变量表格与批量编辑器只是同一份数据的两种投影。
- **替代而非并存：** SoloDock 不实现第二套 Compose 规范，只生成受限 schema，并把权威校验和执行交给已安装的 Compose CLI。
- **适度抽象：** 单进程、单 Rust crate、单 service 应用、SQLite、SSE 和全局单部署避免个人单机用例不需要的平台抽象。
- **明确失败与原子动作：** Registry、credential、确定性配置、健康、宿主资源和中断错误彼此区分。candidate 持久化先于 Docker 变更，active 切换晚于健康验证。
- **影响范围审计：** 镜像 digest 语义覆盖 deploy/start/restart/rollback/drift/UI；bind mount allowlist 覆盖配置 schema、validate、Compose 生成、release snapshot、preview、删除和恢复；删除前 preview container、文件、network、保留 volume 和保留 bind mount 源目录。
