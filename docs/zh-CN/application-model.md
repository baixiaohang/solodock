# SoloDock 应用模型

> [English](../application-model.md)（权威版本） · 简体中文

SoloDock 只管理结构化的单 service 应用。管理员填写受支持字段，系统生成并验证 canonical Compose；不存在通用 Compose 导入或原始 YAML 编辑入口。

## 应用、draft 与 project

应用拥有不可变 UUID 与不可变 slug。UUID 是 API path、filesystem directory 和 ownership label 的权威身份；新应用的 slug 必须全局唯一，由 1–20 个小写 ASCII 字母、数字或连字符组成且首尾为字母或数字。slug 创建后不能更新，`display_name` 仍可修改。

资源命名只有一个版本化 helper。Compose project 为 `solodock-<slug>`，默认容器名通常为 `solodock-<slug>-app-1`，owned network 为 `solodock-<slug>-default`，owned volume 为 `solodock-<slug>.<logical-name>`。旧 naming v1 应用继续使用 `sd-<slug>` bridge；新 naming v2 应用使用 UUID 派生的 `sd-<12-char-token>`，从而允许 20 字符 slug 且不改变历史 UFW/nftables identity。不允许用户设置 `container_name` 或任意 Docker resource name。

空白创建只写 app metadata，不伪造空 config revision，也不创建 Docker 资源。此时详情状态为 `UNCONFIGURED`，draft/image/credential/active/pending 均为空且 desired state 为 stopped。第一次保存配置与普通更新复用同一 revision mutation，只有当前没有 draft 时才接受 `expected_revision = null`；deploy/start/poll/webhook 对未配置应用确定性返回 `APP_UNCONFIGURED`。

`app.toml` 指向当前 draft config revision，并记录 desired state、Registry polling 配置和最后一次文件系统 mutation。每次编辑先完整发布新的 `config-revisions/<revision-id>/`，最后原子替换 metadata 作为 commit point。已经发布的 release 固定引用自己的 config revision，因此后续 draft 编辑不会改变 active 或 pending 容器挂载的内容。

draft 还包含应用级 `stop_grace_period_seconds`，范围为 `1–600`，默认 `10`。发布时该值与 config revision 一起固定进 immutable release，并生成 Compose `services.app.stop_grace_period`。它表示 SIGTERM 后允许服务收尾的最长时间，而非固定等待；服务提前退出时 lifecycle 或部署立即继续。缺少该字段的旧 config revision 和 release 按 `10` 秒解释，并继续使用原 canonical hash/HMAC。

## 镜像与 credential

draft 保存一个带 tag 的 discovery image reference。tag 只用于 Registry 发现；实际 release 和 Compose 使用：

```text
<canonical-registry>/<repository>@sha256:<manifest-digest>
```

多平台镜像同时记录 source descriptor、可选 index digest、选中的 OS/architecture/variant、manifest digest 和 image config digest。历史 v2 release 仍以 `local_image_id` 键序列化 config digest，以保持 HMAC 和存量文件兼容。Docker Engine 的 image/container observation 可能以 config digest 或选中的 manifest digest 表示 image ID；SoloDock 用同一身份对象匹配两者。旧/classic daemon 未返回 manifest descriptor 时回退到该 digest 集合。Docker 29 containerd image store 的原始 `ImageInspect.Descriptor` 可能带 digest 但缺 platform；Docker adapter 只用同一次 `ImageInspect` 响应的顶层 OS/architecture/variant 补齐缺项，形成 effective observation，不覆盖 descriptor 已有值。effective descriptor 一旦存在，其 digest 和 canonical platform 仍必须完整匹配，缺字段、格式错误或值不符都 fail closed；`ContainerInspect.ImageManifestDescriptor` 没有该 fallback，始终按自身字段严格校验。应用可引用一个 logical registry 精确匹配的 write-only Registry credential；credential 生命周期见 [部署与回滚](deployments.md)。

## 环境变量

环境变量只有一份规范数据。配置页的 public 环境变量可以在逐行编辑器与批量 `KEY=VALUE` 文本之间无损切换；批量模式按第一个 `=` 分隔，忽略空行，并对缺少分隔符、非法或重复 key 给出行号。Secret 始终留在独立的 write-only 逐行区域，不进入批量文本，也不使用可被误提交的占位值。

- public 值可以读取和编辑；
- 已保存的 secret 只显示“已保存”占位且 value 保持空白；留空、输入新值和删除行分别投影为 `keep`、`replace`、`delete`；
- API、UI、SQLite、release、Compose、audit、错误和 tracing 不回显 secret；
- public/secret 分类转换必须显式删除旧分类并提交新分类；
- 重复 key、非法名称、插值或命令替换语法会被拒绝。

生成的 Compose 不含 secret 原值，只引用权限受限的受管文件。

## 受管文件

受管文本文件包含 logical name、容器 target path、sensitive 标记和只读属性。public 内容可读取；secret 内容使用与环境变量相同的 write-only operation。配置 revision 对单文件和总量设置配额，并将 public/secret 内容存入不同权限边界。

宿主上的 state root、应用目录、config revision 及 `files/{public,secret}` 目录保持 `0700 solodock:solodock`；只有实际 bind mount 的 `files/public/<logical-name>` 与 `files/secret/<logical-name>` direct leaf 是精确 `0444 solodock:solodock`。因此任意常见非 root 容器 UID/GID 可以读取显式挂入自己的文件，而普通宿主用户仍无法穿过私有 ancestor 枚举或读取 state tree。环境 secret、Registry/webhook credential、SQLite、release metadata 和其他控制面文件不使用该例外，继续保持私有文件权限。

所有受管文件仍以 Compose `read_only: true` 挂载，容器不能写回宿主 inode。需要容器写入的持久内容必须使用 volume 或显式确认的 read-write bind，不能借受管文件绕过 secret、配额或不可变 release 语义。每次发布在私有 temp revision 内写完、显式设置最终 mode 并 fsync 后才原子可见；部署前 strict loader 会拒绝 mode、owner、类型或 symlink 漂移，并沿用相应部署阶段的配置或 release 无效错误。

## Port、volume、bind 与 network

### Port

发布 port 必须显式使用 loopback host IP，并区分 TCP/UDP。SoloDock 不接受非 loopback 应用发布地址。

### Named volume

- owned volume 由应用 logical name 映射到内部 Compose resource key；
- external volume 必须预先存在，SoloDock 不改变其 ownership；
- lifecycle、deploy、rollback、unregister、remove 和 deletion 均不传入 volume 删除参数。

### Bind mount

允许 bind 的根目录由 SQLite 全局设置维护，默认空，即默认禁用 bind。升级时 TOML `allowed_bind_roots` 只做一次 bootstrap import；此后 UI/SQLite 是唯一事实源。删除仍被 draft、active 或 pending revision 引用的 root 会返回冲突。启用后：

- root 必须是既有、绝对、无 symlink 的私有安全路径；
- source 必须是 root 的严格子目录，不能直接挂载整个 root；
- root/source 不能与 state、runtime、Docker socket、敏感系统目录或 daemon 实际 data-root 重叠；
- validate、preview 和每次 Docker effect 前都会重新检查 canonical path、symlink、device/inode 和 data-root；
- 默认只读；每一条 read-write bind 都必须在对应行显式确认“不能随 release 回滚”，改回只读或重新切为读写会重置该确认；
- read-write source 不得成为同一配置或其他正在运行的受管应用中任一 bind source 的严格祖先；相同 source 与兄弟 source 不属于祖先冲突；
- SoloDock 不创建、`chown`、`chmod`、备份或删除 source。

保存 draft 时会执行早期祖先检查。每个 start-like effect 都会基于其他受管应用的 fresh live inventory 重复检查。应用替换或 restart 会先停止其 exact owned writer container、确认已退出，再重新验证目标路径，之后才允许启动。跨应用冲突只以 `BIND_SOURCE_ANCESTOR_CONFLICT` 阻止冲突应用的 start、deploy、restart、rollback 或 recovery；SoloDock 不会自动停止另一应用，read、stop 与用于修复配置的 edit 仍保持可用。

### Network

新应用默认同时挂载应用 owned default network 和平台内部服务发现网络。owned network 的 bridge identity 由应用 naming schema 决定；平台网络固定为 internal `solodock-services`、host bridge `sd-services`，并带精确 platform ownership labels。首次需要时由唯一 manager inspect-or-create；同名资源的 driver、internal、bridge 或 labels 不匹配时 fail closed，绝不接管。应用在平台网络上的 DNS alias 为 slug，因此可通过 `<slug>:<container-port>` 互通。平台网络不随单个应用删除。

config schema 1/2 与 release schema 3/4 的 `service_discovery_enabled` 有效值固定为 `false`，即使向旧签名域注入该字段也不能扩大网络权限。旧应用只有显式保存新 revision/release 后才加入。配置还可加入最多 8 个显式存在的 external network，或按高级设置关闭 owned/platform network；若三类网络均关闭则拒绝配置。

每个 external attachment 可为当前唯一 `app` service 配置最多 8 个稳定 alias。Alias 是小写 DNS label，在同一 attachment 内唯一；网络与 alias 都按 canonical 顺序进入 config SHA、release 完整性和 Compose。无 alias 的旧 release 继续使用 Compose networks 短语法，默认布尔值与空 alias 不参与旧 canonical serialization。

External network 必须由管理员预先创建。effect 前的 fresh Docker snapshot 会验证网络存在性、成员 full ID 与有效 DNS names；alias 被无关容器占用、成员观察不完整或发生变化时 fail closed，只有 caller 已精确验证的待替换旧容器 full ID 可被忽略。SoloDock 永不创建、修改或删除 external network；取消注册或移除容器时也保留 owned network。

volume、bind 和受管文件的容器 target path 必须互不冲突。preview 会展示规范化后的 source/target、只读状态以及资源是 owned 还是 external。

## 健康策略

应用选择一种 health policy：

- `healthy`：要求 Docker health 达到 `healthy`，可使用镜像内置或结构化 HTTP healthcheck；
- `running`：同一容器在稳定窗口内持续处于 running，默认 15 秒；
- `completed`：一次性工作负载以退出码 0 完成；
- `disabled`：显式确认降低安全性，只证明容器达到有限运行条件。

健康策略用于 deployment commit 和 rollback verification。更改 draft policy 不会追溯改变既有 release。

健康检查的数值范围与默认值由 Rust domain 常量定义，并通过 `GET /api/v1/settings` 的 `configuration_limits.health` 投影给 Web；前端不复制另一套边界。capability 不可用时配置编辑 fail closed，避免浏览器接受后端必然拒绝的值。

## 生命周期与删除

start、stop、restart 和 remove 只作用于通过 project/service/app/release/schema/full container ID 完整 ownership 校验的对象。stop/restart 使用被操作 release 固定的停机宽限；remove 先以同一宽限显式 stop，再移除已停止容器。unmanaged、stale、multiple 或 malformed candidate 一律 fail closed。

删除是两阶段协议：

1. preview 从 fresh filesystem、active/pending/draft config 和精确 Docker observation 生成 canonical facts；
2. DELETE 提交 confirmation token、slug 和 disposition，并在 token consume 与 tombstone 前重算 facts hash。

preview 合并 active、pending 与 draft 中的文件、volume、bind、network，并区分实际存在与仅配置。Network fact 带 owned/external kind、owned bridge name、排序后的 aliases 和配置 scope；external-only revision 不虚构 owned default network。已配置或 degraded webhook 会保守提示其 write-only secret 随 app tombstone 永久删除。

默认 deletion 只 unregister。显式 remove 也只移除 token 绑定的精确 owned container；所有 volume、bind 内容和 network 继续保留。删除与 stream producer 之间使用可回滚 barrier，只有 filesystem tombstone commit 后才永久阻止该 app 的新流。

部署、active/pending 和 rollback 语义见 [部署与回滚](deployments.md)，恢复时的文件权限与链接约束见 [恢复](recovery.md)。

## 手动 artifact 清理

存储清理是显式的“预览并确认”操作。它始终保护 active/pending release、当前 draft revision、`queued`、`running`、`interrupted` 与 `needs_attention` deployment 的恢复引用、清理恢复 artifact，以及每个应用额外三个最近的回滚 release。每次预览最多选择全局最旧的 100 个已验证且无引用 release；config revision 只有在没有任何保留 release 或 draft 引用时才成为候选。已知私有临时 artifact 复用同一 typed store inventory。未知名称、链接、类型、owner、mode、签名或 ledger 事实都会让整个 inventory fail closed。

清理绝不删除应用 metadata、deployment、audit 历史、credential、container、volume、bind 数据、network、backup 或 operator 管理的 Docker 资源。逻辑大小估算不承诺实际释放的磁盘空间。
