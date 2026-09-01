# insight-agent 一次性迁移

先从仓库部署说明和 live `docker inspect <inspect-before-use>` 记录 full container ID、已发布 digest、端口、环境变量 key、数据库/network、volume/bind、restart和health。SoloDock 不构建源码；必须先在现有发布流程生成镜像。

完成并验证业务数据库与持久路径 restore。已有 `/home/ubuntu/insight-agent/data`、`/home/ubuntu/insight-data` 与 `/home/ubuntu/.codex` 可直接使用 bind：先在系统设置允许 `/home/ubuntu`，再逐行映射到 `/app/data`、`/var/lib/insight-data`、`/home/insight/.codex`。SoloDock 不复制数据或改权限；维护窗口内先停止旧 systemd writer，核对 UID/GID、完整 `.git`、deploy key 与备份，再配置一个 loopback host port、`/readyz` 和 60 秒停机宽限并部署。不得使用 `down -v`、prune、通配删除或移除 bind source；保留旧配置和备份作为人工回退。
