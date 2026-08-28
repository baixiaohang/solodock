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

## 后端开发

运行 API 骨架：

```bash
cargo run
```

服务监听 `127.0.0.1:8080`，并提供 `GET /healthz`。可以通过 `SOLODOCK_LISTEN_ADDR` 改为其他 loopback 地址：

```bash
SOLODOCK_LISTEN_ADDR=127.0.0.1:9090 cargo run
```

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
