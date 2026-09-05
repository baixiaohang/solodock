# SoloDock 运维

> [English](../operations.md)（权威版本） · 简体中文

生产边界先见 [产品范围](product-scope.md)；应用资源、部署状态和 system health 语义分别见 [应用模型](application-model.md)、[部署与回滚](deployments.md) 和 [API 与实时流](api-and-streams.md)。

## 安装与升级

生产目标为 Ubuntu 24.04 x86_64，并需要当前受支持且不低于 28.3.3 的 Docker Engine、`docker` group/socket、systemd 与 Docker Compose v2.24+。稳定 GitHub Release 提供长期保留的 package，需要已登录且具备 `gh attestation verify` 能力的 GitHub CLI；宿主机不需要 Rust、Node.js、npm 或 Git。发行版自带 package 可能滞后于 GitHub CLI 的能力。安装或升级 `gh` 时应使用 GitHub 的[官方 Linux 说明](https://github.com/cli/cli/blob/trunk/docs/install_linux.md)，并以以下能力检查而不是文档中永久写死的版本号作为判断依据：

```bash
gh attestation verify --help
gh auth login --hostname github.com
gh auth status
```

以下 Bash 命令读取 GitHub 的实际 Latest Release，验证它已正式发布且稳定，要求 canonical `vMAJOR.MINOR.PATCH` tag，把 tag 解析为不可变 commit，下载三个精确 asset，验证 `SHA256SUMS` provenance 与 source identity，校验内外两层 checksum，然后安装 package 内 binary：

```bash
set -euo pipefail
repo=baixiaohang/solodock
release_data=$(gh release view --repo "$repo" --json tagName,isDraft,isPrerelease \
  --jq '[.tagName, .isDraft, .isPrerelease] | @tsv')
IFS=$'\t' read -r tag is_draft is_prerelease <<<"$release_data"
[[ $is_draft == false && $is_prerelease == false ]]
[[ $tag =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]
source_sha=$(gh api "repos/$repo/commits/$tag" --jq .sha)
[[ $source_sha =~ ^[0-9a-f]{40}$ ]]
asset="solodock-${tag}-ubuntu-24.04-x86_64.tar.gz"
download_dir=$(mktemp -d)
trap 'rm -rf -- "$download_dir"' EXIT
gh release download "$tag" --repo "$repo" --dir "$download_dir" \
  --pattern "$asset" --pattern SHA256SUMS --pattern SOURCE_SHA
gh attestation verify "$download_dir/SHA256SUMS" \
  --repo "$repo" \
  --signer-workflow "$repo/.github/workflows/release.yml" \
  --source-ref "refs/tags/$tag" \
  --source-digest "$source_sha" \
  --deny-self-hosted-runners
(cd "$download_dir" && sha256sum -c SHA256SUMS)
[[ $(<"$download_dir/SOURCE_SHA") == "$source_sha" ]]
tar -xzf "$download_dir/$asset" -C "$download_dir"
package="$download_dir/solodock-package"
(cd "$package" && sha256sum -c SHA256SUMS)
[[ $(<"$package/SOURCE_SHA") == "$source_sha" ]]
[[ $(<"$package/VERSION") == "${tag#v}" ]]
"$package/verify-package.sh" "$package"
sudo "$package/install.sh" --version "${tag#v}"
```

package 中经过 checksum 绑定的 `INSTALL_MANIFEST` 记录其 `stable` channel、版本、source commit 与完整 package 内容身份。installer 验证 manifest 后，会准备一个不可变且以身份限定的 generation，其中同时包含 binary、manifest、updater、package verifier、backup/restore helper 与 systemd unit。它先快照所有现有公开入口，再切换 helper 和 unit，最后以 `/usr/local/bin/solodock` 作为安装身份 commit marker；事务提交前任何失败都会恢复全部入口并移除不完整 generation，确保可见 binary、helper、unit 与 manifest 始终来自同一个 package。`/usr/local/bin/solodock-restore` 默认解析同 generation 的 `solodock` sibling binary 作为 validator。backup 与 restore 输出必须位于调用管理员拥有、group/other 不可写的 canonical directory；每一级祖先必须由管理员或 root 拥有，任何可写祖先还必须具备 sticky-directory 保护。helper 会把所有临时与发布路径锚定到已检查的目录身份，在发布前后复核 path/device/inode，使用排他且不可预测的临时名，并且绝不替换已有 archive、checksum 或 restore target。除非首次安装显式使用 `--enable-now`，installer 不启动服务，也不覆盖 `/etc/solodock/config.toml` 或 `/var/lib/solodock`；应完成配置与离线备份后再启动。含新 SQLite migration 的升级是 forward-only，不能只切回旧 binary。

官方 packaged profile 把 config 固定为 `/etc/solodock/config.toml`、state 固定为 `/var/lib/solodock`、runtime 固定为 `/run/solodock`。`listen_address` 仍可配置，但必须是显式 loopback socket；public origin 仍可配置为 canonical HTTPS origin。Systemd unit 通过精确的 `SOLODOCK_PACKAGED_LAYOUT=1` 选择该 profile。Runtime 会在创建受管目录或打开 SQLite 前拒绝非法 marker 或路径漂移。Installer、updater、backup 与 restore 都会在自身 mutation boundary 前调用 package generation 中无副作用的 Rust config inspector，不会在 Bash 中解析或跟随任意 TOML 路径。

已有自定义路径的源码部署必须先迁移，才能使用 package updater：停止旧服务，独立备份实际配置的 state 与 config，把完整 state 移到 `/var/lib/solodock` 并设置精确 `solodock:solodock` owner 和文档规定的 mode，把 config 安装到 `/etc/solodock/config.toml` 并设置 `state_directory = "/var/lib/solodock"`、`runtime_directory = "/run/solodock"`，然后先用旧 binary 在固定布局上验收，再运行 `solodock-update`。新安装验收前保留独立的迁移前备份。新版 updater 的 preflight 会在停服务前拒绝不兼容配置，它本身不能代替迁移备份。

### 已验证的 stable 与 main 升级

installer 同时安装 `/usr/local/bin/solodock-update`。先由日常管理员账号完成一次登录；令牌只需具备读取仓库、Release 或 Actions artifact 与 artifact attestation 所需的权限。不要把 token 写入脚本、配置或命令行：

```bash
gh attestation verify --help
gh auth login --hostname github.com
gh auth status
solodock-update
```

不传 `--channel` 时，updater 读取当前带版本的 `INSTALL_MANIFEST`：Release 安装继续使用 `stable`，CI 安装继续使用 `main`。显式传入一次 `--channel stable|main` 可主动换轨；成功安装的新 package 会记录新 channel，之后无参数运行会继续沿用。对没有 manifest 的旧安装，只能从精确的受管 `main-<12 位十六进制>` 或 canonical SemVer 目录推断；其他格式 fail closed，并要求显式 channel。`--branch` 与 `--workflow` 只适用于 main，非法组合会在认证、下载、`sudo` 或服务变更前被拒绝。

`stable` channel 读取 GitHub 的实际 Latest Release，并要求其已正式发布、非 draft、非 prerelease。Release workflow 让 GitHub 使用其版本感知默认规则确定 Latest，不会强制每个后来创建的旧版本线 Release 成为 Latest。如果 GitHub 返回的 stable 版本低于当前已安装 stable manifest，updater 会在下载或 mutation 前拒绝降级。

updater 会先复用已有或免密的 `sudo` 授权；需要密码时才在交互终端提示一次。无 TTY 且未配置非交互 `sudo` 的调用会在修改服务前失败。

stable discovery 下载精确命名的 Ubuntu archive、`SHA256SUMS` 与 `SOURCE_SHA`；它把 Release tag 解析到 commit，要求 canonical tag 与 package version 一致，并针对 `.github/workflows/release.yml`、精确 tag ref、commit 与 GitHub-hosted runner 验证 checksum attestation。main discovery 则选择最新成功的 `push` CI，保留既有 `.github/workflows/ci.yml`、branch、commit 与 GitHub-hosted runner attestation policy。artifact/attestation 缺失或过期、Release identity 变化或非法，以及任何 source、version 或 checksum 不匹配都会 fail closed。

discovery 之后，两个 channel 进入同一个 package validation 与 apply 路径。在停止服务、进行离线备份或调用 installer 前，updater 会用已完整验证的下载 binary 检查 `/etc/solodock/config.toml`；custom layout 或畸形 inspector output 会直接失败，不使用 fallback 值。精确的 IPv4 或带方括号 IPv6 loopback `/healthz` URL 也来自该记录，不再有 `--health-url` override。currentness 要求已安装 manifest 中的完整 package identity、当前不可变 generation、binary、updater、package verifier、backup/restore helper、对应受管 symlink 与 systemd unit 全部和已验证 package 一致。若只有 package identity 或 helper 变化而 binary digest 不变（包括 main→stable），updater 会事务性发布新 generation 并检查运行中服务，不停服务也不调用 binary。binary 确有变化时才停止 SoloDock，在 `/var/backups/solodock/` 创建离线控制面备份，事务性发布 stable SemVer 或 `main-<commit SHA prefix>` generation，启动并检查 loopback `/healthz` 与 `/favicon.svg`。调用新 binary 前安装失败时，只有完整恢复并验收旧 package generation 后，updater 才会重启旧服务；任何 rollback 操作或验收失败都会让 installer 返回可区分的不完整回滚状态、保留事务现场，并由 updater 保持或置为停服并给出人工恢复指引，绝不启动当前残留 link 指向的 binary。新 binary 被调用后则遵守 forward-only 门禁。临时下载在所有退出路径清理，应用容器、volume 和 bind 数据始终不在操作范围内。

认证后，sidebar 会读取 `/api/v1/system/installation` 并显示同一安装身份。stable 安装显示 SemVer、channel 与短 source commit；main 安装显示 `main` 与 source commit。展开条目可查看完整 source SHA 和 package identity，便于提交 issue。endpoint 每次请求都会读取固定受管 symlink 与 manifest，因此 package-only channel 变更无需重启 SoloDock，下一次页面加载即可看到。正常本地源码运行显示 `development`；受管 manifest 缺失、损坏或格式不规范时显示 `unknown`，但不会让其他控制面能力降级。公开 `/healthz` 与未认证登录页不会暴露该指纹。

这是一项管理员显式触发的维护操作，不应直接放入无人值守 timer。新 binary 一旦被尝试启动，健康失败不会自动切回旧 binary，因为 SQLite migration 是 forward-only；此时保留备份和现场，按本页与[恢复](recovery.md)流程检查。channel、repository、main selector 和备份目录选项见 `solodock-update --help`；loopback probe 地址始终来自已安装 config。

GitHub **Release asset** 是长期保留的稳定分发；**Actions artifact** 是 main channel 使用、保留 30 天的开发产物；**GitHub Packages** 是 container 与语言 package 的另一套 registry，SoloDock 不向其发布，也不依赖它。

## 安全前置条件

服务只监听 loopback，`public_origin` 必须是 HTTPS。外部 tunnel 或 reverse proxy、访问控制和 TLS 是部署前置条件，不由 SoloDock 配置。Proxy 必须保留 `public_origin` 的精确外部 `Host`；若改写为 loopback upstream authority，management request 会以 `404` fail closed。`Forwarded`、`X-Forwarded-Host`、`X-Original-Host` 等 header 会被有意忽略，不能提供路由 authority。`solodock` 用户属于 `docker` group；这等同宿主 root 权限，必须限制主机管理员、配置文件和 Web 登录面。

生产环境必须使用当前受支持且不低于 28.3.3 的 Docker Engine。Docker 文档说明，28.0.0 以前的版本可能让同一二层网络中的其他主机访问仅发布到 loopback 的端口；28.3.3 还修复了 CVE-2025-54388，即 firewalld reload 后可能暴露 loopback-published port。参见 Docker 的 [port publishing 安全说明](https://docs.docker.com/engine/network/port-publishing/)和 [28.3.3 release notes](https://docs.docker.com/engine/release-notes/28/#2833)。SoloDock 无法修复 daemon 或宿主 firewall 行为，因此达到版本下限不能替代下面的宿主验收。

生产启用前、每次相关升级或 firewall reload 后，以及 tunnel/proxy 规则变更后，执行以下检查：

1. 运行 `docker version --format '{{.Server.Version}}'` 与 `docker compose version`；确认 daemon 当前仍受支持、版本不低于 28.3.3，且 Compose 不低于 v2.24。
2. 使用 `ss -ltnp`（或宿主等价工具）确认 SoloDock 进程只监听配置的 loopback socket。
3. 检查 `docker ps --all --format '{{.Names}}\t{{.Ports}}'`，并对每个发布端口的应用核对 `docker inspect` 中的 `HostIp`/`HostPort`。每个 bind 必须符合预期 interface；空地址或 `0.0.0.0`/`::` 会对外可达，除非宿主网络策略已明确证明阻断。新增或修改应用端口以及重新部署后，都要重复该检查。
4. 独立于 Docker 自动生成的规则检查宿主 firewall，例如运行 `nft list ruleset`，使用 firewalld 时同时运行 `firewall-cmd --state` 和 `firewall-cmd --list-all-zones`。核对 default policy 与显式 ingress rule，并在 daemon 或 firewall reload 后重新检查。
5. 从同一二层网络中的另一台主机连接所有应保持 loopback-only 的 management/application port；连接必须失败，只在本机测试成功不能证明外部隔离。
6. Management hostname 只路由到 management ingress，保留精确 `public_origin` authority，并用部署侧访问控制保护。Cloudflare 部署还应在严格 WAF 规则之外启用 Access MFA。
7. 为 `webhook_public_origin` 使用独立 hostname，外部 ingress 只允许精确的 `POST /hooks/v1/apps/<canonical-lowercase-UUID>/registry`；拒绝该 hostname 上的 UI/API、所有 GET 与非 canonical path。外部 1 KiB body limit、rate limit 与 concurrency limit 至少应与 SoloDock 文档中的 endpoint 上限同样严格。
8. 不得把来源 IP、`Forwarded` 或 `X-Forwarded-*` 当作 webhook 认证；签名的 body/path/timestamp/nonce 协议才是权威边界。

把 Docker 版本、实际 bind、firewall 结果、外部二层探测与 ingress policy revision 记录到部署验收证据中。Docker、firewalld/nftables、kernel networking、Cloudflare Tunnel、WAF 或 reverse proxy 变更后应重新执行完整清单。

官方 unit 对 Docker 只有 `After=`/`Wants=` 顺序。socket 缺失或 daemon 停止属于受支持的 degraded 状态：SoloDock 继续运行，`/healthz`、认证、filesystem catalog 和 recovery 信息仍可用，需要 Docker 的操作继续返回具体 degraded 错误。若 `/var/run/docker.sock` 已存在，installer 仍要求它是由配置的 `docker` group 拥有的 Unix socket；普通文件、symlink 或错误 group 的 socket 绝不会按 degraded 放行。首次导入非空 legacy TOML bind roots 时，如必须观察 Docker data root 才能证明无重叠，仍保持 fail closed。

启用 webhook 时还需设置不同 authority 的 `webhook_public_origin`，并在外部 WAF 只放行精确的 `POST /hooks/v1/apps/<canonical-lowercase-UUID>/registry`。Webhook authority 会拒绝 UI、management API、GET 和非 canonical path；management authority 会拒绝 webhook path。签名、timestamp/nonce、重试和 202 语义见 [Webhook 说明](webhooks.md)。

首次启动从 `/run/solodock/bootstrap.token` 完成一次性 bootstrap。日常查看：

```bash
systemctl status solodock.service
journalctl -u solodock.service --since today
curl --fail http://127.0.0.1:8080/healthz
```

当 loopback listen authority 与 `public_origin` 不同时，它永久只暴露精确的 `GET /healthz` 与 `GET /favicon.svg`，让已安装 updater 能探测 strict-Host binary；它不暴露登录页、其他 asset、management API、SSE 或 webhook route。直接诊断时应在 request URL 使用已配置 loopback authority，不能用 forwarding header 替代。

认证后的 `/api/v1/system/health` 分开展示 Docker、恢复、projection、存储清理恢复、deployment、poll coordinator、磁盘与 credential 状态。cleanup 状态为 `pending` 表示仍有 exact plan 或 tombstone 需要 replay/finalize；`degraded` 表示其 recovery ledger 无法验证。`interrupted`、`needs_attention` 或 ownership collision 需要先按 deployment detail 与精确 `docker inspect` 处理，不能 prune、宽泛删除或猜测性重跑。

任何引用 external network 的应用都必须先由管理员创建目标 Docker network。SoloDock 不改变该网络的 driver、IPAM、labels 或生命周期；升级、部署与删除也不会移除它。新服务默认使用的 `solodock-services` 不是用户 external network：SoloDock 会在首次需要时创建 internal bridge，并严格校验 `sd-services` 与 platform labels；`PLATFORM_NETWORK_IDENTITY_CONFLICT` 时应先识别同名资源来源，不能让 SoloDock 接管或自动删除。应用可用 slug 和容器端口进行内部访问。

Owned network 的 host interface 以应用详情展示值为准：旧应用可能为 `sd-<slug>`，新应用为 UUID 派生 token。配置 UFW/nftables 前先用详情页、`ip link show` 与 `docker network inspect solodock-<slug>-default` 核对身份；不要自行按 slug 猜 bridge。平台内部网络使用 `sd-services` 且是 internal，不替代应用 owned network 的出网职责。

`NETWORK_BRIDGE_IDENTITY_CONFLICT` 表示既有 owned network 的 driver 或 bridge option 与 canonical identity 不一致。SoloDock 不会删除或接管该 network；停止相关容器、核对 ownership 并由管理员处理冲突资源后，再重新部署。

应用的停机宽限默认 `10` 秒，可在注册或配置页面设置为 `1–600` 秒。它是 SIGKILL 前的最大等待，服务提前退出不会空等。需要 flush 数据、drain 队列或最终同步的应用应按自身关闭契约显式放大；deploy/recreate 停止 predecessor、手动 stop/restart、显式 remove 和失败 rollback 都使用被停止 release 的值。

## 自动部署与凭据

自动部署必须由管理员显式确认启用。开关关闭只阻止未来 poll，不取消已经 durable claim 的部署。`config_pending_manual` 表示 digest 未变但 draft 配置变化，需使用 Deploy；`suppressed_failed_target` 表示该 target 已失败/回滚，先检查 health 和数据兼容，再由新 digest/config 或明确人工部署解除。轮换 Registry credential 会改变 generation 并重新进入带 jitter 的轮询。

磁盘告警时先扩容或清理 SoloDock 之外可确认的无用内容；不得删除 state 内 revision/ledger、Docker volume 或 bind source。`MemoryHigh=256M` 是 soft pressure，没有 `MemoryMax`。

控制台 system health 的“主机内存可用”来自 Linux `/proc/meminfo` 的 `MemAvailable`，与应用容器自身的 memory usage 是两个不同事实。该解析器也被 image pull 前的 128 MiB 内存门禁复用；读取、字段或数值无效时返回 unknown 并使健康状态 degraded，不伪造为 0。

bind allow roots 在“系统设置 → 存储访问”维护。SoloDock 只验证既有绝对目录并授权应用引用，不提供目录浏览，不执行 `mkdir/chown/chmod/rm`。升级前 TOML 值只导入一次；之后应在 Web 修改。若删除被引用的 root 返回 `BIND_ROOT_IN_USE`，先从列出的 draft/active/pending 配置移除 bind 并完成安全迁移。

PostgreSQL 快速部署默认使用 major 18 和 `/var/lib/postgresql` owned volume；选择 17 时目标为 `/var/lib/postgresql/data`。升级 major 不会自动改现有 volume target或迁移数据，必须按 PostgreSQL 官方流程单独备份、迁移和验收。数据库默认不发布宿主端口，其他新服务通过 `<postgres-slug>:5432` 访问。

全局显示时区在 Web“系统设置”中从后端 IANA tzdb 列表选择，保存在 SQLite singleton settings record，默认 `UTC`。修改使用 revision、幂等键、Origin、session 与 CSRF，保存后无需重启即可重绘所有 Web 时间。该设置不向受管容器注入 `TZ`，也不改变数据库、API、SSE、cursor、过期判断或下载日志中的 UTC 原值；浏览器不支持已保存 zone 时会明确告警并按 UTC fallback。

Web UI 可在 bootstrap/login 页面和登录后的 header 中切换 English 与简体中文。SoloDock 只在浏览器 `localStorage` 的版本化非敏感 key `solodock.ui.locale.v1` 中保存显式选择，不把 locale 写入 API、session、audit、URL、SQLite 或 server settings。没有有效已存值时，浏览器第一偏好语言为 `zh` 或 `zh-*` 才选择 `zh-CN`，其他情况使用 English；storage 不可用或值非法时会安全回退，不阻塞 UI。切换会立即更新可见文案、本地化时间、可访问性标签和 document `lang` 属性。

## 备份

停止服务后执行：

```bash
sudo systemctl stop solodock.service
sudo /usr/local/bin/solodock-backup --output /secure/new/solodock-control-plane.tar
```

archive 含应用、Registry credential 和 webhook secret，必须按高敏数据限制读取并另行加密。它保留 immutable revision 中的 network mode 与 aliases，但不包含业务 volume、bind 数据、Docker image/container 或 network；恢复前必须单独重建所需 external network，每个工作负载也必须有独立且验证过 restore 的数据备份。

Backup helper 会从自身不可变 package generation 解析 `solodock` binary，并在创建 archive 或临时输出前验证固定 packaged layout。Restore 会在修改 owner/mode 或发布目标前对解出的 config 执行同一检查。自定义路径 config 会被拒绝，不会被部分归档或静默跟随。

恢复 archive 或处理 degraded/interrupted 状态前，按 [恢复](recovery.md) 的 fail-closed 流程操作；安全前提见 [威胁模型](threat-model.md)。

## 手动存储清理

SoloDock owned immutable artifact 请使用 **系统设置 → 存储清理**。先扫描并检查精确的 release/config/temp 清单与回滚损失，再确认并应用。Preview 不是锁：apply 会重新核验所有受保护事实；计划发生变化时零删除拒绝。清理永远不会定时执行，也不会由磁盘阈值触发。除 active/pending/恢复事实外，每个应用还保留三个最近的回滚 release，且绝不触碰业务数据或 Docker 资源。显示的逻辑大小只是估算，不保证实际释放空间。

## 按应用选择容器安全策略

应用配置可选 `security_profile`，例如 `codex-v1`；空值/null 保持 Docker 默认策略。它是**宿主机预装的策略对名称**，并非任意 Compose 安全选项。SoloDock 只给该应用的单个服务生成 `seccomp=/etc/solodock/security-profiles/<name>/seccomp.json` 和 `apparmor=solodock-<name>`。名称以小写字母开头，长 1–48 个字符，仅含小写 ASCII 字母、数字或连字符；拒绝 `unconfined`。不会添加 capabilities、宿主机 namespace 或 privileged 模式。

宿主机管理员须在部署前安装两份策略、加载 AppArmor，并配置开机加载。策略文件应归 root 所有且不能由业务容器写入。SoloDock 不上传、编辑、安装或放宽宿主机策略。缺失或无效的策略会通过已有部署失败路径导致容器创建失败，不回退为 unconfined。Compose 预览不能证明内核兼容性；部署提升前应在目标宿主机运行应用自己的沙箱冒烟检查。

策略名是不可变的版本标识。改变内容时应安装新的版本名，不能覆盖已安装版本。配置 revision schema 4 通过已有完整性哈希保护所选名称；release、重试、恢复、回滚读取同一 revision。只要运行容器或保留的回滚 release 仍引用旧版本，就须保留对应 seccomp 文件与已加载的 AppArmor profile。宿主机策略内容由管理员管理，不复制到 release artifact，回滚依赖原名称对应的策略仍在。清空应用设置并重新部署会恢复 Docker 默认配置，不改变其他应用。
