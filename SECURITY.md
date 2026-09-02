# 安全策略

## 支持范围

SoloDock 当前处于 `0.x` 阶段，只为默认分支的最新版本提供安全修复。历史提交、旧构建产物和自行修改的版本不单独维护。

SoloDock 通过 Docker socket 管理宿主容器；获得 SoloDock 进程权限等同获得宿主 root 级能力。部署时必须保持 loopback 监听，并在外部入口实施 TLS、认证前访问控制和速率限制。

## 报告漏洞

请使用 GitHub 仓库 Security 页面中的 **Report a vulnerability** 私下提交安全问题：

<https://github.com/baixiaohang/solodock/security/advisories/new>

不要在公开 Issue、Pull Request 或 Discussion 中披露尚未修复的漏洞、利用步骤、真实凭据或生产环境标识。报告应尽量包含受影响版本、前置条件、最小复现、影响范围和建议缓解方式。

维护者会尽力确认报告、评估影响并协调修复与披露时间；当前项目不承诺固定响应时限。若 GitHub 私密漏洞报告尚未启用，请先通过维护者已有的私密联系渠道报告，不要创建公开 Issue。

## 不属于漏洞的情况

- 在管理员主动授予 Docker socket、宿主 bind root 或 Registry credential 后获得相应能力；
- 已在文档中明确说明的单管理员、单主机、非多租户边界；
- 仅造成版本信息、公开配置字段或通用部署架构可见，且不包含秘密或访问能力；
- 没有可复现安全影响的自动化扫描结果。
