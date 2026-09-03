# SoloDock 测试与安全护栏

> [English](../testing.md)（权威版本） · 简体中文

SoloDock 的测试目标不是只证明 happy path，而是证明 Docker root 级控制面在失败、中断、ownership drift 和数据保留场景中仍然 fail closed。

## 分层

- 单元测试：domain validation、reference/parser、HMAC、redactor、state machine、clock/backoff、path 和 filesystem helper；
- integration/API：临时目录中的 SQLite、filesystem-first publication、认证、幂等、删除、recovery 与 typed response；
- frontend：dotenv、write-only retry identity、deployment/poll state、credential、逐行 port/storage/health/managed-file editor、结构化 network editor、preset 两阶段恢复和 destructive preview组件；
- embedded/package smoke：production asset、真实 HTTP bootstrap/login/API、installer upgrade、systemd、backup/restore；
- Registry + Docker E2E：独立 private Bearer Registry 和 Docker-in-Docker daemon，穿过 production HTTP、poll/webhook、scheduler、pull、Compose、health、rollback 和 cleanup 边界；
- resource harness：production embedded binary、60 秒 idle sample、60 秒 authenticated SSE 和独立 dockerd采样。

测试数量会随实现演进，文档只固定场景和护栏，不把某次运行的计数作为长期契约。

非认证 integration/API 与 Docker E2E fixture 直接写入格式有效、期限受控的测试管理员与 session，不重复执行生产参数 Argon2。认证 API 仍完整覆盖 bootstrap、密码 hash/verify、login、cookie、CSRF、logout、revoke 和 audit；AuthService 的生产参数 bootstrap/login 单元路径也保留。测试 session helper 不进入 production binary，且保留 bootstrap/login 两条 fixture audit，使业务用例的审计计数语义不变。

普通 PR 由 `classify` 区分纯文档变更与代码变更。代码变更会并行运行 Web 检查、Rust lint、Rust tests、release/package smoke 和安全策略检查；只有明确的文档、Web 或 CI 安全工具路径可以跳过 DinD，未识别的非文档路径默认运行 Docker E2E。`ci-gate` 按分类结果逐项核对所有分支，只接受预期的成功或安全跳过，并拒绝意外 skip、失败或取消的 run；`main` push 始终运行完整检查。安全策略检查会结构化解析 workflow，验证 action 固定版本、危险触发器与权限、依赖差异，以及 Rust advisory、license 和来源策略；CodeQL 在独立 workflow 中分析 Rust 与 JavaScript/TypeScript。PR 上 classic suite 使用 1 秒 SSE hold 验证连接与 permit 释放；相关 PR 的 Docker 29 job 继续运行 descriptor deployment/no-op 与两个 compensation 场景。完整资源窗口由每周一及手动触发的 `Extended CI` 运行，后者也周期性复验三个 `containerd_` 场景。

## Docker 隔离

Docker/Compose E2E 必须使用隔离 daemon 或显式 test-only endpoint。生产代码固定 `/var/run/docker.sock`；仅 `docker-e2e` feature 可把 runner连接到测试 daemon。

相关 PR 同时运行 Docker 27 classic image store 的完整 DinD 回归和固定 Docker 29.7.2 的三个 `containerd_` 场景，覆盖 descriptor deployment/no-op、pre-marker 错误 claim 清理与 post-marker replacement 现场保留；`Extended CI` 周期性复验同一 Docker 29 套件。两个 daemon 模式都有 backend 硬断言：classic job 拒绝 `io.containerd.snapshotter.v1`，containerd job 必须观察到该 snapshotter，不能把两个 job 静默跑成同一存储模式。classic DinD job 把 workspace 下的专用 fixture root 以同一绝对路径挂入 daemon service；managed-file bind source 必须位于该 root，不能依赖 daemon 看不到的 runner 临时路径，也不能挂载生产 Docker socket。三个 `containerd_` 场景均受场景总 deadline、deployment/gate deadline 和 shutdown deadline 约束；成功、失败、panic 或超时后都按 exact app/project/full ID 和 ownership label 清理本场景 container、network 与已声明 volume，job 级 timeout 只作最后保险。

每次运行生成唯一 project/run token，并记录所有 container、volume、network 和临时 bind source 的精确 ID。cleanup 前重新 inspect full ID、label 与 run token，finally 只删除本次创建的对象。

测试禁止：

- 挂载 CI/开发宿主的生产 Docker socket给被测控制面；
- `docker system prune`；
- 通配 container/image/volume/network 删除；
- `docker compose down -v` 或任何 volume 删除参数；
- 扫描并删除不带本次 run token 的对象；
- 把真实业务服务、数据库、管理工具或其 volume/network 放入 selector。

bind fixture 必须位于本次测试私有临时根；cleanup 不得把“数据保留成功”误实现为删除 canary source。

## 核心验收场景

### 身份与 secret

- bootstrap 至多一次、Origin/CSRF/session/revoke/heartbeat；
- public/secret 分类和 `keep`/`replace`/`delete`；
- public 环境变量逐行/批量 `KEY=VALUE` 无损切换，覆盖 CRLF、空行、第一个 `=`、重复/非法 key 与行号错误；Secret 不进入文本框，并继续覆盖掩码、分类转换、rename、keep/replace/delete 和成功后清空；
- 镜像建议 POST 的 JSON `Content-Type`、CSRF、credential reference、allowlist 成功投影及脱敏错误展示；
- 配置字段级 `issues` 可定位对应区块/行，响应与 UI 不泄露 public 值、Secret、credential 或宿主路径；
- Registry/webhook secret write-only、zeroize、rotation/revoke/finalizer；
- secret canary 不进入 API、SSE、audit、tracing、error、Compose、release、SQLite 或 argv；
- degraded inventory保留旧 redactor，冷启动不完整 inventory fail closed。

### Filesystem 与恢复

- temp、rename、parent fsync 和 visible-effect failpoint；
- runtime read-only scan不删除并发 writer artifact；
- startup-only cleanup只处理 canonical、ledger-owned artifact；
- active/pending canonical symlink、mode/owner、HMAC、config/release/Compose验证；
- public/secret managed leaf 精确 `0444`、ancestor 保持 `0700`，restrictive umask 下发布 mode 不收窄；startup 仅迁移 canonical legacy `0400`/`0600`，runtime scan 不改权限，unsafe drift fail closed；
- SQLite 丢失后的可重建事实和不可伪造的认证/audit历史；
- backup/restore拒绝 escaping link、hard link、special file和不兼容 state。

### Docker 与 Compose

- project/service/schema/app/release/full ID ownership；
- 新应用 1–20 字符不可变 slug、旧应用 12 字符边界，以及版本化 project/container/network/volume/bridge identity；
- owned network 的版本化 bridge option、effect 前 identity conflict、observer expected/actual projection，以及删除重建后 identity 稳定性；
- `UNCONFIGURED` create/replay、首次 nullable revision、deploy/start/poll/webhook fail closed 且不创建 Docker 资源；
- 平台 internal network inspect-or-create、同名漂移拒绝、旧 release 不自动加入、两个应用以 slug 跨服务连接；
- PostgreSQL 18/17 major-specific volume target、Secret 不回显、创建/部署双幂等与部分失败恢复；
- OCI config blob 大小/media type/digest、allowlist 投影和 Env/labels/command 不回显；
- unmanaged、stale、multiple、replacement collision在 runner前 fail closed；
- canonical YAML、`.env` 隔离、固定 argv、禁 shell/exec/pull/build/down/volume removal；
- owned-only、owned+external、external-only 的 canonical YAML，旧无 alias 短语法逐字节兼容，以及 typed alias 长语法；
- external network 缺失、无关成员 alias 冲突、精确 predecessor full ID 放行、不完整成员 observation fail closed；
- active/pending immutable network expectation、attachment/alias drift 和 Docker 自动 DNS names 子集语义；
- bind allowlist、symlink/device/inode/data-root revalidation，以及每条 read-write bind 未确认、只读切换和重新确认；
- HTTP health 五组数值范围与运行稳定窗口由 settings capability 驱动，Web 与 Rust domain 边界一致且 capability 缺失时 fail closed；
- SQLite bind roots 一次 bootstrap、revision 更新、引用保护与扫描失败关闭；
- lifecycle、deploy、rollback、unregister和remove后的volume/bind/network canary保留。
- `/proc/meminfo` 正常、缺失、非法和 overflow，system health 五列状态条与 pull 门禁共用同一 parser；
- 固定非 root image 同时读取 public/secret managed file，首次部署、第二 revision、manual rollback 和 strict recovery 成功，restart count 不增长且容器写入 readonly mount 失败；
- external-only 不生成、检查或展示 owned bridge identity。

### Registry 与 deployment

- public/private Bearer auth、exact scope、401/403/TLS taxonomy；
- parent/child digest、manifest media type和canonical platform；
- classic image store 的 config ID 与 descriptor-absent 兼容路径；Docker 29.7.2 containerd image store 必须硬断言原始 `ImageInspect.Descriptor` digest 存在、platform 缺失且顶层 OS/architecture 完整，adapter 形成 effective observation 后首次部署和同 release no-op 均成功；descriptor 错误、冲突或补全后仍不完整继续 fail closed；
- resolve→pull之间tag移动仍运行已解析digest；
- candidate durable-before-effect；首次 post-effect observation 用唯一非 predecessor full ID 和全套 canonical candidate-release labels 建立 ownership claim，并写入 exact `post_container_id`；
- 停机宽限默认值、`1..=600` 边界、Compose `stop_grace_period`、stop/restart argv、stop-before-remove、predecessor/candidate 各自 release 值，以及缺字段旧 config/release 的 canonical hash/HMAC 兼容；
- 全局时区默认 UTC、IANA allowlist、revision conflict、幂等 replay、Origin/CSRF/audit，以及 UTC、Asia/Shanghai 和 DST zone 的显示；API/SSE 原值与 expiry/cursor 保持 UTC；
- deployment history 桌面一项一行、移动端不并排，并保留可访问的详情链接；
- pre-marker canonical candidate claim 后的 semantic mismatch 进入确定性补偿；post-marker 不同 full ID 才是 replacement，必须保留 pending/替代容器且不能伪造 `failed`/`rolled_back`；
- 首次部署的 remove 失败、remove 后 observation 失败或仍有 container 必须保留 pending 和原始 `candidate_failed` history，只能记录 `CANDIDATE_CLEANUP_FAILED`，不能写 `failed`；
- health failure自动恢复、manual rollback和rollback failure；
- candidate 创建后的确定性身份拒绝会在首次部署证明移除、在已有 active 时恢复并健康复核旧 release；
- timeout/shutdown/unknown effect保持interrupted并由fresh exact facts收敛；
- poll no-op、busy coalescing、backoff、ETag generation隔离和failed-target suppression；
- production coordinator heap/dispatch、durable webhook wake、cancel和TaskTracker join。

完整 DinD 验收还应快速部署 PostgreSQL，用第二个新应用通过 `<slug>:5432` 写入 canary，Recreate 后再次读取；不得为了测试暴露 PostgreSQL host port。具体业务应用的既有宿主目录、UID/GID、deploy key 与旧单 writer 切换仍属于生产维护窗口人工验收，不进入 CI fixture。

### 删除

- preview合并active/pending/draft和degraded webhook facts，并按 network kind/aliases/scope 保留 external-only 差异；
- token hash在consume和tombstone前重验；
- slow resource inventory后再次验证container candidate；
- stream barrier rollback/commit和producer join；
- visible tombstone、projection failure、durable response和background/startup finalizer。

## 资源验收

正式资源场景记录 commit、kernel、cgroup、工具链、warm-up/sample窗口、binary size、RSS/CPU/FD/task、control-plane峰值、dockerd峰值和metadata大小。8条 authenticated SSE 在 `Extended CI` 正式窗口中保持60秒并于窗口末端采样，drop后StreamGate permit必须归零；普通 PR 仅保留短窗口回归 smoke。

目标、报告格式和当前基线见 [资源预算](resource-budget.md)。这些结果是本地/CI回归基线，不冒充真实生产主机测量。

## 文档变更验证

纯文档 PR 至少执行：

```bash
git diff --check
rg -n "proposals/" README.md README.zh-CN.md docs --glob '!testing.md' --glob '!AGENTS.md'
```

并人工检查：

- 所有相对 Markdown 链接目标存在；
- 每个英文专题文档都有同名 `docs/zh-CN/` 翻译，反之亦然；
- README 能导航到当前专题；
- 文档事实与对应 code/schema/test 一致；
- 没有重新引入已完成 milestone、计划目录、固定测试计数或第二套事实源；
- diff 只包含本任务授权的文档。

日常开发命令和默认验证边界见仓库根 `AGENTS.md`；运维验收与恢复演练分别见 [运维](operations.md) 和 [恢复](recovery.md)。
