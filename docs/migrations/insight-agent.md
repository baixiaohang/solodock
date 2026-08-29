# insight-agent 一次性迁移

先从仓库部署说明和 live `docker inspect <inspect-before-use>` 记录 full container ID、已发布 digest、端口、环境变量 key、数据库/network、volume/bind、restart和health。SoloDock 不构建源码；必须先在现有发布流程生成镜像。

完成并验证业务数据库与持久路径 restore。已有 named volume/network声明为 external，bind纳入 allowlist；在 create/validate/deletion preview逐项核对。维护窗口内停止旧容器，以 digest部署并验证业务、依赖和 canary。不得使用 `down -v`、prune、通配删除或移除 bind source；保留旧配置和备份作为人工回退。
