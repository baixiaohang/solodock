# SoloDock 应用模型

SoloDock 只管理结构化的单 service 应用。管理员填写受支持字段，系统生成并验证 canonical Compose；不存在通用 Compose 导入或原始 YAML 编辑入口。

## 应用、draft 与 project

应用拥有不可变 UUID 与不可变 slug。UUID 是 API path、filesystem directory 和 ownership label 的权威身份；创建时输入的 slug 必须全局唯一，由 1–12 个小写 ASCII 字母、数字或连字符组成且首尾为字母或数字。slug 创建后不能更新，`display_name` 仍可修改。

所有可读 Docker 名称只从 slug 派生：Compose project 为 `solodock-<slug>`，默认容器名通常为 `solodock-<slug>-app-1`，owned network 为 `solodock-<slug>-default`，owned volume 为 `solodock-<slug>.<logical-name>`，Linux bridge 为 `sd-<slug>`。不持久化 project name，也不允许用户设置 `container_name` 或任意 Docker resource name。`.` 不属于 slug 合法字符，因此它为 owned volume 的 slug 与 logical name 提供无歧义边界。

`app.toml` 指向当前 draft config revision，并记录 desired state、Registry polling 配置和最后一次文件系统 mutation。每次编辑先完整发布新的 `config-revisions/<revision-id>/`，最后原子替换 metadata 作为 commit point。已经发布的 release 固定引用自己的 config revision，因此后续 draft 编辑不会改变 active 或 pending 容器挂载的内容。

## 镜像与 credential

draft 保存一个带 tag 的 discovery image reference。tag 只用于 Registry 发现；实际 release 和 Compose 使用：

```text
<canonical-registry>/<repository>@sha256:<manifest-digest>
```

多平台镜像同时记录 source descriptor、可选 index digest、选中的 OS/architecture/variant、manifest digest 和 image config digest。历史 v2 release 仍以 `local_image_id` 键序列化 config digest，以保持 HMAC 和存量文件兼容。Docker Engine 的 image/container observation 可能以 config digest 或选中的 manifest digest 表示 image ID；SoloDock 用同一身份对象匹配两者。旧/classic daemon 未返回 manifest descriptor 时回退到该 digest 集合。Docker 29 containerd image store 的原始 `ImageInspect.Descriptor` 可能带 digest 但缺 platform；Docker adapter 只用同一次 `ImageInspect` 响应的顶层 OS/architecture/variant 补齐缺项，形成 effective observation，不覆盖 descriptor 已有值。effective descriptor 一旦存在，其 digest 和 canonical platform 仍必须完整匹配，缺字段、格式错误或值不符都 fail closed；`ContainerInspect.ImageManifestDescriptor` 没有该 fallback，始终按自身字段严格校验。应用可引用一个 logical registry 精确匹配的 write-only Registry credential；credential 生命周期见 [部署与回滚](deployments.md)。

## 环境变量

环境变量只有一份规范数据，UI 的表格和 dotenv 批量编辑是其不同投影。

- public 值可以读取和编辑；
- secret 值只能以 `keep`、`replace`、`delete` 显式操作；
- API、UI、SQLite、release、Compose、audit、错误和 tracing 不回显 secret；
- public/secret 分类转换必须显式删除旧分类并提交新分类；
- 重复 key、非法名称、插值或命令替换语法会被拒绝。

生成的 Compose 不含 secret 原值，只引用权限受限的受管文件。

## 受管文件

受管文本文件包含 logical name、容器 target path、sensitive 标记和只读属性。public 内容可读取；secret 内容使用与环境变量相同的 write-only operation。配置 revision 对单文件和总量设置配额，并将 public/secret 内容存入不同权限边界。

所有受管文件只读挂载。需要容器写入的持久内容必须使用 volume 或显式确认的 read-write bind，不能借受管文件绕过 secret、配额或不可变 release 语义。

## Port、volume、bind 与 network

### Port

发布 port 必须显式使用 loopback host IP，并区分 TCP/UDP。SoloDock 不接受非 loopback 应用发布地址。

### Named volume

- owned volume 由应用 logical name 映射到内部 Compose resource key；
- external volume 必须预先存在，SoloDock 不改变其 ownership；
- lifecycle、deploy、rollback、unregister、remove 和 deletion 均不传入 volume 删除参数。

### Bind mount

`allowed_bind_roots` 默认空，即默认禁用 bind。启用后：

- root 必须是既有、绝对、无 symlink 的私有安全路径；
- source 必须是 root 的严格子目录，不能直接挂载整个 root；
- root/source 不能与 state、runtime、Docker socket、敏感系统目录或 daemon 实际 data-root 重叠；
- validate、preview 和每次 Docker effect 前都会重新检查 canonical path、symlink、device/inode 和 data-root；
- 默认只读，read-write 必须显式确认“不能随 release 回滚”；
- SoloDock 不创建、`chown`、`chmod`、备份或删除 source。

### Network

新应用默认创建并挂载 slug 派生的 owned default network。其 Compose definition 固定使用 `bridge` driver，并设置 `com.docker.network.bridge.name=sd-<slug>`，因此 network 删除重建后 host interface identity 不变。配置也可同时加入最多 8 个显式存在的 external network，或关闭 owned default 后仅使用 external network；external-only 模式至少需要一个 attachment。External definition 不写 driver option。

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

## 生命周期与删除

start、stop、restart 和 remove 只作用于通过 project/service/app/release/schema/full container ID 完整 ownership 校验的对象。unmanaged、stale、multiple 或 malformed candidate 一律 fail closed。

删除是两阶段协议：

1. preview 从 fresh filesystem、active/pending/draft config 和精确 Docker observation 生成 canonical facts；
2. DELETE 提交 confirmation token、slug 和 disposition，并在 token consume 与 tombstone 前重算 facts hash。

preview 合并 active、pending 与 draft 中的文件、volume、bind、network，并区分实际存在与仅配置。Network fact 带 owned/external kind、owned bridge name、排序后的 aliases 和配置 scope；external-only revision 不虚构 owned default network。已配置或 degraded webhook 会保守提示其 write-only secret 随 app tombstone 永久删除。

默认 deletion 只 unregister。显式 remove 也只移除 token 绑定的精确 owned container；所有 volume、bind 内容和 network 继续保留。删除与 stream producer 之间使用可回滚 barrier，只有 filesystem tombstone commit 后才永久阻止该 app 的新流。

部署、active/pending 和 rollback 语义见 [部署与回滚](deployments.md)，恢复时的文件权限与链接约束见 [恢复](recovery.md)。
