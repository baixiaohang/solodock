# SoloDock 部署与回滚

SoloDock 的 manual deploy、poll auto-deploy 和 rollback 共用唯一的 `DeploymentScheduler` 与 deployment engine。外部 webhook 只生成 durable Registry recheck wake，不形成第二条 Registry、Docker 或 Compose mutation 路径。

应用页只读取最近 20 条 deployment，每个 deployment 以一整行展示显示时区下的创建时间、status/phase、trigger、镜像或 digest 和错误码，并通过明确链接进入详情；移动端只允许单项内部换行，不会把两条历史并排。

## Registry credential

Registry credential 以 filesystem-first 方式保存在 `registry-credentials/<credential-id>/`：metadata 与 immutable secret revision 分离并受完整性校验。API 只返回 registry、username 和 revision 等 metadata，不回显 token。

创建、轮换和删除都使用幂等 ledger 与 operation-owned artifact。删除先保留 exact tombstone，在成功响应持久化后由 API、background reconciler 或 startup finalizer 精确清理。任何 draft、active/pending 或历史 release 仍引用 credential 时，删除 fail closed。

pull 只在 `/run/solodock/docker-config/<deployment-id>/config.json` 创建 operation-scoped Docker config。该目录和本项目持有的 credential buffer在所有退出路径清理/zeroize；清理不确定时 deployment 进入需要处理的安全故障，不用原 pull 错误掩盖。

## 从 tag 到不可变 release

1. 严格解析并 canonicalize discovery reference、logical registry、repository 和 tag；
2. 按 OCI Distribution Bearer challenge 获取精确 `repository:<repo>:pull` scope；
3. 校验响应 media type、header/body digest、manifest/index descriptor 和本机 canonical platform；
4. 记录 source descriptor、index/manifest digest 和 OS/architecture/variant；
5. 只执行 digest-pinned pull；Docker adapter 先把原始 `ImageInspect` 投影为 effective observation，其中 descriptor 的缺失平台字段只能由同一响应的顶层平台补齐，再用 canonical repository digest、config/manifest image identity、有效 manifest descriptor 和顶层 platform 复核结果；
6. 在任何 Docker effect 前写入 immutable release、canonical Compose 并设置 `pending`。

tag 在 resolve 之后移动不会改变已经调度的 candidate。SoloDock 当前不验证 Cosign/Sigstore 签名，因此可信 Registry 或账户被攻陷仍属于供应链风险。

## 调度与状态

deployment ledger 的终态为：

- `succeeded`：candidate 已通过健康门禁并成为 active；
- `no_op`：目标 release 与 exact active/pending/actual facts 已收敛，无 Docker effect；
- `failed`：Docker effect 前或已经证明无未知副作用的确定性失败；
- `rolled_back`：candidate 正常失败后，旧 active 已重新 apply 并通过健康门禁；
- `needs_attention`：自动恢复也失败，或安全清理/事实不确定，需要管理员判断；
- `interrupted`：shutdown、timeout、unknown Compose/Docker effect 或现场 drift，系统不猜测结果。

非终态 deployment 为 `queued` 或 `running`，并通过 resolving、preparing、pulling、applying、verifying、committing、rolling-back 等 phase 提供可观测进度。phase 是 ledger 进度，不是允许外部调用者绕过 fresh facts 直接续跑的命令入口。

同一个 app 同时只能有一个 mutation；全局 Compose/deployment effect 也受限。schedule transaction 持久化稳定的 `202 Accepted` receipt、expected active/pending/actual facts、deployment、transition 和 audit。`202` 只表示任务已 durable 接受，不代表部署成功。

## Candidate apply 与 active commit

每条 Compose effect 路径在 durable effect marker 后重新读取并验证：

- current app metadata、active/pending link 和 immutable release；
- HMAC、config revision、digest image 和 canonical Compose；
- daemon 实际 data-root、volume/network ownership 和 bind identity；
- Compose project/service 下的全部 container candidate 与精确 predecessor。

最后一个 Docker await 完成后调用固定 Compose action。替换已有容器时，先使用 predecessor release 自己固定的停机宽限完成 stop，确认旧 writer 已退出后才启动 candidate；candidate 的新宽限只约束其后续关闭。若 candidate 尚未创建就失败，系统会先恢复已停止的 predecessor，再按确定性或不确定性结果收敛。unmanaged、stale、replacement、multiple candidate 或 resource drift 都不会进入 runner。

Compose 后的首次 observation 是 ownership claim boundary：唯一非 predecessor full container ID 与全套 canonical project/service/app/schema/candidate-release labels 足以证明 owned candidate，并立即把 exact ID 作为 `post_container_id` 写入 ledger；configured digest reference、config/manifest image identity、可用的 manifest descriptor、platform、status 和 health 属于后续 release validity。首次 marker 前，即使 Docker daemon/root 主体复制全部 canonical labels 重建容器，当前信任模型仍会 claim 该唯一容器；这种具备 daemon 控制权的行为不在 threat model 内，系统不提供 Compose effect 因果 attestation。

上述 `ImageInspect` 补全只属于 pull 后的 adapter normalization；candidate、health、no-op、failed apply 和 rollback 读取的 `ContainerInspect.ImageManifestDescriptor` 不使用顶层 fallback，继续要求 descriptor 自身的 digest 与 canonical platform 完整匹配。

`post_container_id` 持久化后，该 exact full ID 成为补偿、health、commit/rollback 的 SSOT。后续观察到不同 full ID 才定义为 uncertain replacement：保留 pending 与现场，进入 `interrupted` 或 `needs_attention`，禁止 stop/remove。这样 deterministic semantic mismatch 不会丢失安全补偿句柄，同时 marker 后 replacement 不会被猜测性清理。

candidate 达到 health policy 后才把 `active` 原子切向该 release，并清理对应 `pending`。active rename、pending unlink、parent fsync 和 desired-state publication由可重放 finalizer收敛；不能因后续 metadata 失败倒退已经可见的 active。

## 失败恢复与回滚

确定性 candidate identity/apply/health 失败且现场被证明属于该 candidate 时，系统进入同一补偿路径：有旧 active 时先用 candidate release 的宽限停止失败 candidate，再自动恢复并重新验证旧 release；没有旧 active 时先按 candidate 宽限显式 stop，再执行精确 `rm --force` 并复核 container 已不存在。rollback 重新执行旧 release 所需的 digest pull、resource/bind/data-root/candidate preflight、fixed Compose action、post-observation 和健康门禁，而不是直接信任历史 YAML 或旧 container。

首次部署没有旧 active 时，失败清理只针对已经 claim 并写入 `post_container_id` 的 exact candidate；remove 结果和最终 absence 必须确认。任何 unknown effect、ownership collision，或 marker 后不同 full ID 的 replacement 都保留 pending/现场并标记 `interrupted` 或 `needs_attention`。

终态不变量是：`failed` 表示 candidate side effect 已被证明不存在，`rolled_back` 表示旧 active 已重新 apply 并通过 fresh identity/health verification；补偿执行或复核失败只能进入 `needs_attention`/`interrupted` 并保留 pending 与 exact recovery facts，不能伪装成已清理的终态。所有补偿和 rollback 都保留 volume、bind 内容与 network，不传 volume 删除参数。

人工 rollback 同样创建新的 deployment，并绑定 fresh active/pending/actual facts。回滚只恢复 release 镜像与生成配置，不回滚数据库 migration、volume 或 bind 内容。

## Registry polling

每个启用 auto-deploy 的 app 具有 generation；它覆盖 draft/source、credential revision、开关和 interval。单一 `PollCoordinator` 维护有界 due heap，Registry resolve 最多并发 2：

- busy app 不排队，留待后续轮询；
-相同 generation/target 的 queued/running deployment合并；
-相同 digest 且 active/pending/actual 与配置均收敛时记录 no-op；
-digest 未变但 draft config 变化时标记 `config_pending_manual`；
-失败 target 按 generation/target durable suppression；
-Registry/transient/credential 错误使用分类 deadline、jitter 和有界退避；
-source/generation 变化清空旧 ETag 和 observed target，不能跨 Registry 复用 validator；
-exact-owned interrupted pending/actual 可由后续 poll 创建新的 durable convergence attempt；unknown ownership 仍 fail closed。

开关关闭只阻止未来 poll，不取消已经 durable claim 的 deployment。

## Webhook recheck

签名 webhook只表示“配置的 tag 可能变化”。有效请求原子写入 nonce claim、audit 和 per-app wake sequence；coalesced sequence 唤醒上述 PollCoordinator，并继续遵守 auto-deploy disabled、backoff、busy、suppression、drift 和健康门禁。

协议、Host 隔离和 WAF 约束见 [Webhook](webhooks.md)。应用资源模型见 [应用模型](application-model.md)，人工处置见 [运维](operations.md) 和 [恢复](recovery.md)。
