# SoloDock 当前架构

> [English](../architecture.md)（权威版本） · 简体中文

SoloDock 是单主机、单管理员、单 service 的容器部署控制面。它保持单 Rust 进程、单 crate 和单一 Docker mutation路径，不引入内部微服务、通用 workflow engine 或第二套 Compose 规范。

```text
Browser
  -> external Tunnel / WAF / TLS
  -> loopback SoloDock process
       |-- REST + bounded SSE + embedded UI
       |-- private filesystem app/release/credential/secret store
       |-- SQLite auth/audit/idempotency/deployment/poll ledger
       |-- Docker Engine API for observation
       |-- fixed docker compose CLI for exact mutation
       `-- OCI Registry client for tag-to-digest resolution
```

产品能力和非目标见 [产品范围](product-scope.md)，具体配置资源见 [应用模型](application-model.md)。

## 组件职责

- Axum API：认证、typed DTO、mutation 协调、SSE 与嵌入式前端；
- app store：应用 metadata、immutable config revision、release、active/pending link、webhook secret；
- SQLite：认证、session、audit、幂等、deployment transition、poll/replay operational state；
- Docker observer：固定 socket上的 capability、list/inspect/events/logs/stats 和 ownership；
- Compose adapter：从 typed config生成canonical单 service YAML，并以固定参数向量执行封闭动作；
- Registry adapter：canonical reference、Bearer auth、manifest/digest/platform resolve；
- deployment engine：manual、poll和rollback的唯一调度/执行状态机；
- PollCoordinator：有界due heap、backoff、coalescing和durable webhook wake；
- projection/reconciliation：从filesystem全量事实刷新catalog、redactor和SQLite查询投影。

生产 binary 使用 `embed-ui` 编译期嵌入 Vite产物，不运行 Node服务。hashed asset可长缓存，HTML不缓存；`/api/**`和`/hooks/**`不会进入SPA fallback。接口边界见 [API 与实时流](api-and-streams.md)。

## 唯一事实来源

| 事实 | 权威来源 |
| --- | --- |
| app metadata、draft config、managed files、credential引用 | 私有文件系统 |
| Registry/webhook/app secret原值 | 专用权限受限文件 |
| immutable release、digest、platform、canonical Compose | 私有文件系统 |
| active/pending release | app目录中的canonical symlink |
| container实际状态、full ID、image和resource存在性 | Docker daemon |
| tag当前指向 | Registry；调度后以release manifest digest为准 |
| 管理员、session、audit、幂等、deployment执行和poll/replay状态 | SQLite |
| 全局显示时区与 bind allow roots | SQLite global settings |
| catalog/redactor/查询索引 | 从上述事实派生的可重建投影 |

任何SQLite投影都不能反向覆盖filesystem事实。SQLite丢失后可以从文件恢复app/release查询事实，但不能伪造管理员、session、audit或deployment历史；必须重新bootstrap。

App header 持久化 UUID、不可变 slug 与 `resource_name_schema_version`，不持久化可派生 project name。domain naming helper 是 project、owned network、owned volume 和 bridge 名称的唯一来源；旧 schema 保留 slug bridge，新 schema 使用 UUID token bridge。只持有 UUID 的异步路径必须重新读取已验证 metadata/catalog，不能猜测资源名。

## 持久化布局

默认根目录如下；权限、HMAC和canonical entry由startup/recovery验证：

```text
/etc/solodock/config.toml

/var/lib/solodock/
  state.sqlite3
  secrets/idempotency.key
  registry-credentials/<credential-id>/
    credential.toml
    secret-revisions/<revision-id>/token
  registry-credentials/.trash/<credential-id>-<operation-id>/
  apps/<app-id>/
    app.toml
    webhook.toml
    webhook-secret-revisions/<revision-id>/
    config-revisions/<revision-id>/
      config.toml
      env/
      files/
    releases/<release-id>/
      release.toml
      compose.yaml
    active -> releases/<release-id>
    pending -> releases/<release-id>

/run/solodock/
  bootstrap.token
  locks/<app-id>.lock
  compose/<operation-id>/
  docker-config/<deployment-id>/config.json
```

具体文件名可能随兼容 migration演进；调用者不得绕过store或recovery直接编辑这些artifact。

## Filesystem-first publication

config revision和release先在同一父目录内写入operation-owned temp，完成file `fsync`、atomic rename和parent `fsync`后才可引用。app metadata或`active`/`pending`link是相应可见事实的commit point。

filesystem commit之后才发布内存catalog/redactor和SQLite投影。投影失败不能把已经提交的事实误报为回滚；系统标记degraded并由shutdown-aware reconciler重试。redactor只有在完整读取所有app、active/pending/draft、Registry credential和webhook secret后才允许destructive replace，不完整时保留旧pattern或拒绝冷启动。

破坏性recovery cleanup只在HTTP listen前运行。运行期verified loader、catalog refresh和reconciler使用read-only scan，不能删除另一个writer正在发布的temp或尚未被旧metadata引用的新revision。

## Docker 与 Compose 边界

生产观察固定连接 `/var/run/docker.sock`，不读取 `DOCKER_HOST`。Docker unavailable时认证控制面仍可启动，catalog和health显示degraded；需要Docker的stream或mutation在effect前失败。

Compose production runner固定执行 `/usr/bin/docker`，清空继承环境，禁用隐式 `.env`，不使用shell。它只能生成version/validate/start/recreate/deploy-candidate/stop/restart/remove的封闭argv；不存在build、pull、exec、down、volume remove或用户参数透传。

每次effect前都从filesystem重新验证active/pending release、config/HMAC/canonical YAML，并枚举project/service下全部container candidate。任一unmanaged、stale、invalid、replacement或multiple collision都fail closed。统一的 canonical network plan 同时驱动 Compose、resource preflight、runtime drift、删除 facts 和 API projection；active/pending expectation 分别来自各自 immutable config revision，不能由 mutable draft 替代。未配置应用没有 revision/release，所有 Docker effect 都在资源创建前以 `APP_UNCONFIGURED` 结束。

Owned network 仅在对应 revision 启用时检查版本化 exact name/ownership、`bridge` driver 和 bridge option；ownership 冲突与 `NETWORK_BRIDGE_IDENTITY_CONFLICT` 都在 runner 前 fail closed。平台内部网络由全局 manager 在首次 deployment effect 前创建或校验，固定 internal/bridge/labels，既不伪装为 external network，也不由应用 deletion 删除。External network 使用 fresh network inspect 加有界并发的成员 container inspect，读取 full ID 与有效 DNS names；缺失、alias 冲突或不完整 observation 均在 runner 前 fail closed。resource、network、bind和daemon data-root在durable marker后再次检查；最后一个外部事实检查完成后才调用runner。

内置 preset 只负责把少量输入渲染成普通 `DraftInput`，不生成 Compose、不直接操作 Docker。OCI metadata inspection 同样是无副作用 reader，复用 Registry auth/redirect/timeout/digest 校验并只返回 UI 消费的 allowlist。两者最终都回到唯一 config revision、release resolver 与 deployment engine。

## 发布与自动化

manual、poll和rollback进入同一个deployment engine。candidate在Docker effect前落盘并设置`pending`；Compose 后的首次 observation 是 ownership claim boundary：唯一非 predecessor full ID 与全套 canonical project/service/app/schema/candidate-release labels 足以证明 exact owned effect，worker 立即把该 ID 持久化为 `post_container_id`，再校验 configured digest reference、config/manifest identity、可用的 manifest descriptor、status 与 health。marker 持久化后该 exact ID 是补偿、health、commit/rollback 的 SSOT；此后不同 full ID 才属于 uncertain replacement，必须保留 pending/现场并进入`interrupted`或`needs_attention`，禁止 stop/remove。exact owned candidate 的确定性拒绝统一进入移除或旧 active 恢复补偿，只有补偿结果被重新观察证明后才写`failed`或`rolled_back`。

webhook HMAC验证后，把nonce claim、audit和per-app wake sequence在一个SQLite transaction提交。sequence只是bounded coalescing signal；PollCoordinator仍会重新读取filesystem、Registry和Docker事实。完整状态语义见 [部署与回滚](deployments.md) 和 [Webhook](webhooks.md)。

## 数据与恢复边界

unregister/remove/delete保留named/external volume、bind内容和network。业务数据不属于control-plane backup，release rollback也不回滚数据migration。安装、备份和故障处置分别见 [运维](operations.md)、[恢复](recovery.md) 和 [威胁模型](threat-model.md)。
