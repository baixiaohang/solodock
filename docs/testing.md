# SoloDock 测试与安全护栏

SoloDock 的测试目标不是只证明 happy path，而是证明 Docker root 级控制面在失败、中断、ownership drift 和数据保留场景中仍然 fail closed。

## 分层

- 单元测试：domain validation、reference/parser、HMAC、redactor、state machine、clock/backoff、path 和 filesystem helper；
- integration/API：临时目录中的 SQLite、filesystem-first publication、认证、幂等、删除、recovery 与 typed response；
- frontend：dotenv、write-only retry identity、deployment/poll state、credential、结构化 network editor 和 destructive preview组件；
- embedded/package smoke：production asset、真实 HTTP bootstrap/login/API、installer upgrade、systemd、backup/restore；
- Registry + Docker E2E：独立 private Bearer Registry 和 Docker-in-Docker daemon，穿过 production HTTP、poll/webhook、scheduler、pull、Compose、health、rollback 和 cleanup 边界；
- resource harness：production embedded binary、60 秒 idle sample、60 秒 authenticated SSE 和独立 dockerd采样。

测试数量会随实现演进，文档只固定场景和护栏，不把某次运行的计数作为长期契约。

## Docker 隔离

Docker/Compose E2E 必须使用隔离 daemon 或显式 test-only endpoint。生产代码固定 `/var/run/docker.sock`；仅 `docker-e2e` feature 可把 runner连接到测试 daemon。

每次运行生成唯一 project/run token，并记录所有 container、volume、network 和临时 bind source 的精确 ID。cleanup 前重新 inspect full ID、label 与 run token，finally 只删除本次创建的对象。

测试禁止：

- 挂载 CI/开发宿主的生产 Docker socket给被测控制面；
- `docker system prune`；
- 通配 container/image/volume/network 删除；
- `docker compose down -v` 或任何 volume 删除参数；
- 扫描并删除不带本次 run token 的对象；
- 把真实 SoloGrove、PostgreSQL、pgAdmin、insight-agent 或业务 volume/network放入 selector。

bind fixture 必须位于本次测试私有临时根；cleanup 不得把“数据保留成功”误实现为删除 canary source。

## 核心验收场景

### 身份与 secret

- bootstrap 至多一次、Origin/CSRF/session/revoke/heartbeat；
- public/secret 分类和 `keep`/`replace`/`delete`；
- Registry/webhook secret write-only、zeroize、rotation/revoke/finalizer；
- secret canary 不进入 API、SSE、audit、tracing、error、Compose、release、SQLite 或 argv；
- degraded inventory保留旧 redactor，冷启动不完整 inventory fail closed。

### Filesystem 与恢复

- temp、rename、parent fsync 和 visible-effect failpoint；
- runtime read-only scan不删除并发 writer artifact；
- startup-only cleanup只处理 canonical、ledger-owned artifact；
- active/pending canonical symlink、mode/owner、HMAC、config/release/Compose验证；
- SQLite 丢失后的可重建事实和不可伪造的认证/audit历史；
- backup/restore拒绝 escaping link、hard link、special file和不兼容 state。

### Docker 与 Compose

- project/service/schema/app/release/full ID ownership；
- unmanaged、stale、multiple、replacement collision在 runner前 fail closed；
- canonical YAML、`.env` 隔离、固定 argv、禁 shell/exec/pull/build/down/volume removal；
- owned-only、owned+external、external-only 的 canonical YAML，旧无 alias 短语法逐字节兼容，以及 typed alias 长语法；
- external network 缺失、无关成员 alias 冲突、精确 predecessor full ID 放行、不完整成员 observation fail closed；
- active/pending immutable network expectation、attachment/alias drift 和 Docker 自动 DNS names 子集语义；
- bind allowlist、symlink/device/inode/data-root revalidation；
- lifecycle、deploy、rollback、unregister和remove后的volume/bind/network canary保留。

### Registry 与 deployment

- public/private Bearer auth、exact scope、401/403/TLS taxonomy；
- parent/child digest、manifest media type和canonical platform；
- resolve→pull之间tag移动仍运行已解析digest；
- candidate durable-before-effect、marker后final facts recheck；
- health failure自动恢复、manual rollback和rollback failure；
- timeout/shutdown/unknown effect保持interrupted并由fresh exact facts收敛；
- poll no-op、busy coalescing、backoff、ETag generation隔离和failed-target suppression；
- production coordinator heap/dispatch、durable webhook wake、cancel和TaskTracker join。

### 删除

- preview合并active/pending/draft和degraded webhook facts，并按 network kind/aliases/scope 保留 external-only 差异；
- token hash在consume和tombstone前重验；
- slow resource inventory后再次验证container candidate；
- stream barrier rollback/commit和producer join；
- visible tombstone、projection failure、durable response和background/startup finalizer。

## 资源验收

正式资源场景记录 commit、kernel、cgroup、工具链、warm-up/sample窗口、binary size、RSS/CPU/FD/task、control-plane峰值、dockerd峰值和metadata大小。8条 authenticated SSE 在正式默认与CI中保持60秒并于窗口末端采样，drop后StreamGate permit必须归零。

目标、报告格式和当前基线见 [资源预算](resource-budget.md)。这些结果是本地/CI回归基线，不冒充真实生产主机测量。

## 文档变更验证

纯文档 PR 至少执行：

```bash
git diff --check
rg -n "proposals/" README.md docs --glob '!testing.md' --glob '!AGENTS.md'
```

并人工检查：

- 所有相对 Markdown 链接目标存在；
- README 能导航到当前专题；
- 文档事实与对应 code/schema/test 一致；
- 没有重新引入已完成 milestone、计划目录、固定测试计数或第二套事实源；
- diff 只包含本任务授权的文档。

日常开发命令和默认验证边界见仓库根 `AGENTS.md`；运维验收与恢复演练分别见 [运维](operations.md) 和 [恢复](recovery.md)。
