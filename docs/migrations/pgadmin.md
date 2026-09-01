# pgAdmin 一次性迁移

先执行 `docker inspect <inspect-before-use>` 并记录 full container ID、digest/image、loopback port、环境变量 key 名、restart/health、network 和 `/var/lib/pgadmin` 的实际 volume/bind；不得猜容器或 volume 名。单独备份并验证 pgAdmin 数据 restore，不要把 PostgreSQL 数据库本身混入本迁移。

将已存在 named volume和 PostgreSQL network声明为 external；bind source 先在“系统设置 → 存储访问”加入允许根目录，SoloDock 不浏览、创建、chown或删除它。在服务详情使用逐行 storage/network editor 和只读 Compose preview 核对目标、读写模式与 retained disposition。维护窗口内停止旧容器，按 digest部署新 SoloDock project并验证登录、内部 `<postgres-slug>:5432` 连接、loopback入口和持久 canary。禁止 `docker compose down -v`、prune、`docker volume rm` 或删除 bind source；保留旧配置与业务备份作为人工回退。
