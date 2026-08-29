# SoloGrove 一次性迁移

迁移前用 `docker inspect <inspect-before-use>` 和当前部署文档记录 full container ID、digest、内部端口、Cloudflare入口、Registry credential引用、依赖 network，以及 data/workspace 的实际 volume/bind。先离线备份并验证 SoloGrove data、agent workspace与依赖数据库 restore；SoloDock control-plane backup不能代替这些业务备份。

已有 volume/network声明为 external，host路径加入严格 bind allowlist。用 create/validate/deletion preview核对全部路径与 disposition。维护窗口内停止旧容器，以 digest部署新 project，验证 Web、agent workspace、依赖和持久 canary。SoloDock不会 adopt旧 project；不得执行 `down -v`、prune、通配cleanup或删除 workspace/data。保留旧容器配置与完整业务备份作为人工回退。
