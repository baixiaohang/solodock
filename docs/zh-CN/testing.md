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

非认证 integration/API 与 Docker E2E fixture 直接写入格式有效、期限受控的测试管理员与 session，不重复执行生产参数 Argon2。认证 API 仍完整覆盖 bootstrap、密码 hash/verify/rotation、login、cookie、CSRF、logout、revoke 和 audit；AuthService 的生产参数 bootstrap/login/password-rotation 路径也保留。测试 session helper 不进入 production binary，且保留 bootstrap/login 两条 fixture audit，使业务用例的审计计数语义不变。

普通 PR 由 `classify` 区分纯文档变更与代码变更。代码变更会并行运行 Web 检查、Rust lint、Rust tests、release/package smoke 和安全策略检查；只有明确的文档、Web 或 CI 安全工具路径可以跳过 DinD，未识别的非文档路径默认运行 Docker E2E。`ci-gate` 按分类结果逐项核对所有分支，只接受预期的成功或安全跳过，并拒绝意外 skip、失败或取消的 run；`main` push 始终运行完整检查。安全策略检查会结构化解析 workflow，验证 action 固定版本、危险触发器与权限、隔离的 Release attestation/publish job、固定 version tag trigger、精确的 Docker 28.5.2 classic DinD 基线、依赖差异，以及 Rust advisory、license 和来源策略；tag gate 还会拒绝非 canonical tag 或与 Cargo package version 不一致的 tag。packaging fixture 让 stable 与 main 都穿过共享 apply path，证明 manifest 跟随安装来源与严格 legacy inference，覆盖 package/helper-only 和 same-binary channel transition 不停服务，执行 stable 单调版本 guard，并模拟晚创建 `v0.1.1` 后 GitHub Latest 仍为 `v0.2.0`。capability preflight fixture 会提供缺少 `gh attestation verify` 的 GitHub CLI，要求输出可执行的官方升级指引，并证明 updater 在认证、下载、`sudo`、服务访问或文件系统 mutation 之前退出。installer failure injection 会在 package-only 与停服路径中覆盖每个 staged generation asset 和公开 link commit point，要求普通失败后的四个公开入口、unit、manifest 与 API 可见 identity 仍属于同一个 package，并保留 forward-only 的调用前门禁。rollback-operation injection 还会分别让 binary commit marker、一个 helper 与 unit 的恢复失败；每次都必须返回不完整回滚状态、保留现场并禁止 `start solodock.service`。release-generation ELF stamp 为 legacy binary-only updater 提供一次安全 delta，使其进入 package-aware updater。CodeQL 在独立 workflow 中分析 Rust 与 JavaScript/TypeScript。PR 上 classic suite 使用 1 秒 SSE hold 验证连接与 permit 释放；相关 PR 的 Docker 29 job 继续运行 descriptor deployment/no-op 与两个 compensation 场景。完整资源窗口由每周一及手动触发的 `Extended CI` 运行，后者也周期性复验三个 `containerd_` 场景。

## Docker 隔离

Docker/Compose E2E 必须使用隔离 daemon 或显式 test-only endpoint。生产代码固定 `/var/run/docker.sock`；仅 `docker-e2e` feature 可把 runner连接到测试 daemon。

相关 PR 同时运行固定 Docker 28.5.2 classic image store 的完整 DinD 回归和固定 Docker 29.7.2 的四个 `containerd_` 场景，覆盖 descriptor deployment/no-op、pre-marker 错误 claim 清理、post-marker replacement 现场保留和保守手动镜像清理；`Extended CI` 周期性复验两种 daemon 基线。两个 daemon 模式都有 backend 硬断言：classic job 拒绝 `io.containerd.snapshotter.v1`，containerd job 必须观察到该 snapshotter，不能把两个 job 静默跑成同一存储模式。classic DinD job 把 workspace 下的专用 fixture root 以同一绝对路径挂入 daemon service；managed-file bind source 必须位于该 root，不能依赖 daemon 看不到的 runner 临时路径，也不能挂载生产 Docker socket。四个 `containerd_` 场景均受场景总 deadline 约束；deployment 场景还限制 gate 与 shutdown deadline；成功、失败、panic 或超时后都按 exact app/project/full ID 和 ownership label 清理本场景 container、network 与已声明 volume，job 级 timeout 只作最后保险。

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
- Web transport normalization 在解析 HTML、空 body 或畸形 body 前处理 `401`，保留经过约束的安全 request ID 且不显示原始 response，并在 logout/revoke-all 的所有未确认失败中保持 authenticated 状态、阻止操作重叠；
- 密码轮换覆盖 Origin、CSRF、session、JSON shape/body limit、共享 cooldown、当前密码错误 HTTP 403、精确 cookie expiry、旧/新密码 login 与脱敏 audit metadata；failure injection 证明 credential hash、全部既有 session、throttle 与 audit 一同回滚且不使 cookie 过期；并发旧密码 login 无法在成功轮换后留下 session，既有 SSE 在下一次 heartbeat 关闭；
- Settings 密码表单只发送当前与新密码，不使用幂等或自动重试；mismatch/policy 错误不发 request，确定性错误保持 authenticated，确认成功才返回 login，模糊 transport 结果既不泄露密码 canary，也不伪报成功；
- Dashboard generation 会 abort 被替代的 load，阻止迟到响应发布状态或打开 SSE，关闭旧的和构造到一半的 source 集，并把 live stats source 限制为八条。Deployment detail polling 同时最多保留一个 request 和一个 timer，以有上限的指数退避重试瞬时错误，成功后恢复一秒节奏，并在 terminal、路由切换或 teardown 时停止；
- 前端 mutation retry 测试覆盖普通 request body、write-only secret digest 与手工管理的 lifecycle/deletion key。Network、未通过校验的 HTML/proxy 错误和 HTTP 5xx 保留 exact key；通过校验的 backend JSON 4xx 与确认成功会清除 key。Secret canary 不进入 error、UI 或浏览器 storage；
- Authority routing 测试通过 production middleware 覆盖 management、webhook 和独立 local-probe authority，包括 DNS/default-port 与 IPv4/IPv6 规范化、URI/`Host` 一致性、缺失/重复/非法/未知/仅 forwarding 输入、精确 webhook method/path/UUID、携带合法 session 的跨 surface request、body-minimal no-store 拒绝，以及最小 health/favicon probe 契约；
- Package layout 测试通过 production Rust inspector 与 runtime marker 覆盖固定 config/state/runtime 身份、自定义 IPv4 与带方括号 IPv6 loopback probe、非法 marker 和 effect 前拒绝。Package fixture 证明 custom layout 或畸形 inspector output 时 installer/updater/backup/restore 零 mutation、updater 无 override URL 派生、Docker socket 缺失与非法的区别，以及 unit 对 Docker 只有 `After`/`Wants` 顺序；
- public/secret 分类和 `keep`/`replace`/`delete`；
- public 环境变量逐行/批量 `KEY=VALUE` 无损切换，覆盖 CRLF、空行、第一个 `=`、重复/非法 key 与行号错误；Secret 不进入文本框，并继续覆盖掩码、分类转换、rename、keep/replace/delete 和成功后清空；
- 镜像建议 POST 的 JSON `Content-Type`、CSRF、credential reference、allowlist 成功投影及脱敏错误展示；
- 配置字段级 `issues` 可定位对应区块/行，响应与 UI 不泄露 public 值、Secret、credential 或宿主路径；
- Registry/webhook secret write-only、zeroize、rotation/revoke/finalizer；
- proof-aware idempotency cleanup：current webhook proof retention、旧创建 proof 已过期后的 rotate/revoke recovery、确定性的 inventory/transition 串行化、finalizer 失败超过 24 小时后的 restart 收敛、canonical pre-metadata crash 保留、metadata/revision/route/operation/status/response identity 异常时 fail closed，以及 100 条 terminal batch 上限；
- secret canary 不进入 API、SSE、audit、tracing、error、Compose、release、SQLite 或 argv；
- degraded inventory保留旧 redactor，冷启动不完整 inventory fail closed。

### Filesystem 与恢复

- temp、rename、parent fsync 和 visible-effect failpoint；
- runtime read-only scan不删除并发 writer artifact；
- startup 与 runtime filesystem scan 都保留 cleanup candidate；只有 exact ledger-owned finalizer 可以移除已认证的 recovery payload；
- active/pending canonical symlink、mode/owner、HMAC、config/release/Compose验证；
- public/secret managed leaf 精确 `0444`、ancestor 保持 `0700`，restrictive umask 下发布 mode 不收窄；startup 仅迁移 canonical legacy `0400`/`0600`，runtime scan 不改权限，unsafe drift fail closed；
- SQLite 丢失后的可重建事实和不可伪造的认证/audit历史；
- backup/restore 拒绝 escaping link、hard link、special file，以及不兼容或非 package state。可用 root 的全链测试在私有 staging root 下存放 fixture，同时为 `/var/lib/solodock` 生成 canonical Compose，再以 service identity 校验 relocated tree。

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
- English 与简体中文 dictionary 在编译期保持 key 同构；locale 测试覆盖首次访问浏览器语言检测、显式已存偏好、刷新持久化、非法或不可用 storage 回退、立即切换、本地化时间和 document `lang` 属性；
- installation identity parser 只接受固定受管 symlink 和 canonical manifest 字段；API 认证与 Web 展示覆盖 stable、main、development、unknown/失败回退、双语 label 及完整 source/package detail；
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

## 手动存储清理门禁

- Protection inventory 覆盖 active、pending、当前 draft、三个 distinct 最近回滚 candidate、`queued`、`running`、`interrupted` 与 `needs_attention` deployment 的全部 release/revision 字段，以及 cleanup recovery proof。
- 损坏的 filesystem/ledger/trash 输入、stale preview 与 busy 有序 app guard 都证明零 token consumption、零 rename。
- 既有 application tombstone 复用删除恢复 validator：pending/interrupted/succeeded proof 可与清理共存；unknown entry、marker 损坏及 proof 缺失或不匹配会阻止 preview，并在消费 token 或 rename 前拒绝先前已签发的计划。
- 最后一份退休 marker unlink 后故障仍保留 durable retirement intent、pending health 与超期 terminal proof，GC 不得回收。Restart 分别覆盖 marker 缺失和模拟崩溃后合法签名 marker 再出现；只有确认目录同步后才能释放 proof。
- 真实 router 测试注入 plan audit transaction 回滚、marker publication 失败、rename 后两个父目录各自的同步失败、item progress 与 response 写入失败。payload 移除、marker 退休和空目录移除后的部分失败均接续新 store/startup scan、proof-aware GC 与统一 finalizer。CI 显式运行 feature-gated filesystem fault 测试，不调用宿主 Docker。
- 真实 rollback scheduler/puller 交错在清理中断时建立新 recovery 引用，exact resume 保留它；busy 与 session 不匹配的重试保留原 operation 的恢复身份。public/secret managed leaf 在 preview、detach 与 finalization 中保持按类型判定的 `0444` 权限规则。
- Release detach 会保留 shared/draft/失败 release 的 config revision；cleaned deployment detail 以 `ROLLBACK_ARTIFACT_CLEANED` 失去 rollback，普通损坏不会被误标。
- Web 测试覆盖 mount 时零请求、acknowledgement、partial result、unknown outcome exact replay、generation disposal 与 token/path 不泄漏。意外 204/202 或格式不符的 200 成功响应会保留精确 retry identity，直到收到已验证的终态结果。

## 手动镜像清理门禁

定向 `m3_api image_cleanup::` 用例走 production router 和真实 artifact cleanup 来源：running/stopped × managed/unmanaged 四类容器、普通保留 release、fresh race、app/Compose guard、非法选择、不完整 identity/inventory 和持久 ledger 损坏。真实有副作用的 daemon mock 与 SQLite trigger 覆盖 remove 前失败、remove 响应丢失、删除后 inspect 失败、progress/response commit 失败、audit 回滚及显式 restart/retry，只能删除精确选中镜像。

`image_cleanup_adapter` 使用 loopback HTTP daemon fixture，不调用宿主 Docker，检查完整无过滤 container 枚举、inspect 缺失失败、exact full ID、`force=false`、`noprune=true`、冲突保留和 absence 确认。CI 显式以 `docker-e2e` feature 运行。隔离 classic/containerd 的 `manual_image_cleanup` E2E 在私有 fixture registry 创建独有 commit 镜像、发布签名 release、通过 artifact API 清理，再通过 image API 仅删除选中的合格镜像。未选镜像、running/stopped unmanaged container、volume/network canary 保留；teardown 只处理记录的精确 fixture 资源。这些宿主 Docker 测试由 CI 执行，不属于默认本地定向验证。

Web 测试覆盖 mount 零请求、两个默认 false 门禁、network error/意外 204/202/畸形 200 后 exact body/key 重试、known stale/partial、安全本地错误与卸载后迟到响应。
