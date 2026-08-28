# SoloDock

SoloDock 是面向个人 Docker 工作负载的轻量级单机容器部署控制台。

> [!WARNING]
> SoloDock 仍处于早期开发阶段，尚不适合管理生产工作负载。

SoloDock 将提供聚焦的 Web 界面，用不可变镜像 Digest 部署预构建容器镜像、检查应用健康状态，并在新版本失败时回滚。它不构建源码、不管理域名或 TLS、不提供反向代理，也不编排多台主机。

本仓库是私有项目，未授予公开使用、复制或分发许可。

## 环境要求

- Rust stable（edition 2024），包含 `rustfmt` 和 `clippy`
- Node.js 24 和 npm
- 计划中的生产环境为 Ubuntu 24.04 和 Docker Compose

当前脚手架不会访问 Docker。后续版本将使用 Docker socket。访问 `/var/run/docker.sock`（包括通过 `docker` group）在效果上等同宿主 root 权限，不能把它视为安全边界。

## 配置与后端开发

SoloDock 只从 `SOLODOCK_CONFIG_PATH` 指定的 TOML 文件加载宿主配置，未设置时使用 `/etc/solodock/config.toml`。配置必须使用 loopback IP 监听地址和 HTTPS 公共 origin。开发时可以复制示例并将两个受管目录改到仅当前用户可访问的绝对路径：

```bash
cp packaging/solodock.toml.example /tmp/solodock.toml
chmod 600 /tmp/solodock.toml
# 编辑 state_directory、runtime_directory 和 public_origin
SOLODOCK_CONFIG_PATH=/tmp/solodock.toml cargo run
```

服务启动时会创建权限为 `0700` 的 state/runtime 目录。首次启动时，`<runtime_directory>/bootstrap.token` 包含一次性初始化 token；token 不会输出到日志。把 token 和密码分别读入 shell 变量后，可以避免把真实值写进命令历史：

```bash
read -r SOLODOCK_BOOTSTRAP_TOKEN < /path/to/runtime/bootstrap.token
read -rs SOLODOCK_ADMIN_PASSWORD
export SOLODOCK_BOOTSTRAP_TOKEN SOLODOCK_ADMIN_PASSWORD
jq -n '{bootstrap_token:env.SOLODOCK_BOOTSTRAP_TOKEN,password:env.SOLODOCK_ADMIN_PASSWORD}' |
  curl --request POST https://solodock.example.com/api/v1/auth/bootstrap \
    --header 'Content-Type: application/json' \
    --data-binary @-
unset SOLODOCK_BOOTSTRAP_TOKEN SOLODOCK_ADMIN_PASSWORD
```

登录必须携带与 `public_origin` 精确匹配的 `Origin`，响应设置 Secure session 和 CSRF cookie。后续认证 mutation 还必须把 CSRF cookie 值放入 `X-CSRF-Token`。认证 API 不使用 `Idempotency-Key`。以下示例同样从交互式输入读取密码，并把 cookie jar 创建为仅当前用户可读：

```bash
read -rs SOLODOCK_ADMIN_PASSWORD
export SOLODOCK_ADMIN_PASSWORD
SOLODOCK_COOKIE_JAR="$(mktemp)"
chmod 600 "$SOLODOCK_COOKIE_JAR"
jq -n '{username:"admin",password:env.SOLODOCK_ADMIN_PASSWORD}' |
  curl --request POST https://solodock.example.com/api/v1/auth/login \
    --header 'Origin: https://solodock.example.com' \
    --header 'Content-Type: application/json' \
    --cookie-jar "$SOLODOCK_COOKIE_JAR" \
    --data-binary @-
unset SOLODOCK_ADMIN_PASSWORD
# 使用结束后删除 "$SOLODOCK_COOKIE_JAR"
```

SQLite 保存管理员凭据、session、登录节流和真实审计历史；应用及 release 的权威事实保存在文件系统，`active` symlink 是 active release 的唯一事实源。SQLite 丢失后，启动扫描会重建应用查询索引，但不能恢复管理员、session 或审计历史，因此必须重新执行 bootstrap。损坏的 SQLite 不会被自动替换。

运行后端验证：

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo build --release
```

## 前端开发

```bash
cd web
npm ci
npm run dev
```

运行前端验证：

```bash
npm run check
npm run build
```

Node/npm 只用于开发和构建 Web UI。最终生产版本会把静态资源嵌入 Rust 二进制，不需要运行 Node 服务。
