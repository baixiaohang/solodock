# SoloDock 威胁模型

## 信任边界

SoloDock 假定唯一管理员和宿主 OS 可信；Registry、镜像、容器输出、Docker/Compose 输出、浏览器输入和反向代理输入均不可信。Docker socket 和 `docker` group 在效果上等同宿主 root，因此 Web 身份认证、loopback 监听、固定动作与 systemd hardening 是纵深防御，不是对恶意宿主管理员的隔离。

secret 为 write-only：本项目拥有的 buffer 会 zeroize，secret 不进入 API response、SQLite、release、audit、argv、tracing 或错误；日志使用完整、fail-closed 的动态已知 secret 集脱敏。内核、allocator、Docker daemon、Registry 服务端和管理员业务备份不在“内存中从未出现明文”的保证范围。备份含 secret，必须高敏保护。

部署只信任严格解析并校验 digest/header/body/platform 的 Registry 结果，不验证 Cosign/Sigstore 签名。tag race 不能改变已调度 candidate，但 Registry/镜像供应链仍可能提供恶意内容。容器 capabilities、mount 和 Compose 由 typed generator 限制；bind allowlist 和 Docker data-root overlap 每次 effect 前 fail closed。

资源防护包括请求/body/stream/log buffer 上限、单一 Compose mutation、最多两个 Registry resolve、poll jitter/backoff、busy coalescing 和失败 target suppression。它不承诺抵御拥有宿主 root、Docker daemon控制权或合法管理员 session 的攻击者，也不提供多租户隔离。

volume、bind 和 external network 不会被自动删除，但其中数据也不会随 release 回滚。应用级备份/restore、安全更新和 schema兼容由管理员负责。
