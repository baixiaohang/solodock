# AGENTS.md

SoloDock 是面向个人单机环境的轻量 Docker 应用部署控制台。一个 SoloDock 应用只对应一个容器和一个预构建镜像；系统生成最小 Compose 配置，不提供源码构建、通用 Compose 导入、反向代理或多节点编排。

## 语言与文档

- 仓库文档、Issue、PR、Review、Release Note 及其他面向人的协作内容默认使用简体中文。
- Commit Message 遵循 Conventional Commits：类型与可选 scope 使用英文，subject 和 body 默认使用中文，例如 `feat: 增加应用状态查询`。
- 代码标识符、命令、环境变量、API 路径、错误信息和第三方产品名保持英文；代码注释优先使用中文。
- 架构、API、配置、运维方式或安全边界发生变化时，同步更新对应文档。

## 技术栈与常用命令

- 后端：Rust stable、edition 2024、Axum、Tokio。
- 前端：Svelte、TypeScript、Vite；生产版本最终嵌入 Rust 单一二进制，不运行 Node 服务。

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features

cd web
npm ci
npm run check
npm run build
```

只运行与本次改动直接相关的最小验证；除非维护者明确要求，不主动运行全量或宿主 Docker E2E。

## 开发与安全边界

- 保持单进程、单 Rust crate 和单 service 应用模型，不为未来能力提前增加平台化抽象。
- Docker socket / `docker` group 权限等同宿主 root 权限；不得将其描述为低权限安全边界，也不得暴露给受管容器或 Web API。
- 管理端和应用发布端口都只允许绑定 loopback；MVP 不接受非 loopback 监听或端口映射。
- Secret 原值不得进入 Git、Compose 文件、日志、错误、审计、普通 API 响应或命令行参数；Compose 只允许引用受管 secret。
- 删除应用默认保留 volume；禁止使用 `docker system prune`、通配删除或未经精确确认的破坏性命令。
- Docker/Compose 集成测试必须使用隔离 daemon 或明确的测试 context，并按随机 project、专属 label 和精确 ID 清理。

## 变更约定

- 保持变更范围小且可审查，不提交 `target/`、`web/node_modules/`、`web/dist/` 等生成产物。
- Rust 与 npm lockfile 必须随依赖变更一起提交。
- 新行为应配套最低层级的确定性测试；失败路径、安全边界和破坏性操作优先测试。
- 未经维护者对具体方案的确认，不擅自扩大产品边界或实施 volume/数据迁移。
