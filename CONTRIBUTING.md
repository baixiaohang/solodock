# 贡献指南

感谢你改进 SoloDock。提交改动前，请先阅读 [产品范围](docs/product-scope.md)、[架构](docs/architecture.md) 和 [威胁模型](docs/threat-model.md)，保持单主机、单管理员、单 service 应用模型及现有安全边界。

## 开发流程

1. 从最新 `main` 创建短生命周期分支。
2. 保持改动小而可审查；行为变化应更新最低层级的确定性测试和相关文档。
3. 只运行与改动直接相关的本地验证；Docker E2E 必须使用隔离 daemon 或明确的测试 context。
4. 创建 Pull Request，说明行为、风险和已运行的验证。
5. 等待必需检查通过并解决 review 意见。外部 fork 的 workflow 需要维护者人工批准后才会运行。

常用验证命令见 [README](README.md)。测试并发度不得大于 2。

## 安全要求

- 不要提交密码、token、私钥、真实生产域名/IP、宿主实例路径或客户数据。
- 不要让 Secret 原值进入 Compose、日志、错误、审计、普通 API 响应或命令行参数。
- 不要扩大 Docker socket、非 loopback 监听、任意 host bind、volume 删除或 shell/exec 能力。
- GitHub Actions 必须使用最小权限；第三方 action 必须固定到完整 commit SHA。
- 安全漏洞请按 [安全策略](SECURITY.md) 私下报告，不要创建公开 Issue。

## 提交约定

Commit Message 遵循 Conventional Commits。类型与可选 scope 使用英文，subject 和 body 默认使用简体中文，例如：

```text
fix(auth): 拒绝过期会话
```

仓库采用 [Apache License 2.0](LICENSE)。提交贡献即表示你有权提供该内容，并同意其按仓库许可证发布。
