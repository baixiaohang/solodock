# SoloDock 资源预算

本文件保存预算、测量方法和实测基线；功能级隔离场景与破坏性测试护栏见 [测试与安全护栏](testing.md)。

资源 harness 为 [`scripts/measure-resources.sh`](../scripts/measure-resources.sh)。它使用 `embed-ui` release binary、私有临时 state/config 和 exact PID，生成 JSON，并在失败时只终止本次进程。CI 的共享 runner仅使用 512 MiB idle RSS 宽上限防退化；它不等价于目标生产主机的真实 2C4G 测量。

目标预算：idle RSS 40–100 MiB、idle CPU通常低于 1%、达到 stream gate代表性负载的 RSS增量 10–40 MiB、pull/Compose期间 SoloDock瞬时 100–300 MiB。Docker daemon解压/镜像内存必须单列。binary、UI 与 control-plane metadata 目标为数十 MiB，metadata 不含 image layer和业务数据且低于 100 MiB。

Webhook ingress 固定为 1 KiB body、16 concurrent、bounded global/per-app rate buckets 和每应用一个 coalesced wake sequence；资源验收沿用 control-plane/DinD 上限，并覆盖 burst 后 replay rows、rate map 与 inflight permit 回落，不把通知数量转换为内存队列。

每次正式验收需记录 kernel、CPU/memory cgroup limit、Rust/Node版本、commit、warm-up/采样窗口、binary size、RSS/CPU/FD/task、Docker daemon峰值以及是否 DinD。每周一及手动触发的 `Extended CI` 产出三份正式机器可读报告：`resource-report.json` 使用 production embedded binary 完成 60 秒 warm-up 与 60 秒 idle CPU 采样；`docker-e2e-resource-report.json` 在完整 private Registry/DinD 自动部署、健康失败恢复、人工回滚与数据 canary 场景中，将 8 条 authenticated SSE（4 events、2 logs、2 stats）保持 60 秒，并在窗口末端记录 stream 增量、control-plane 峰值和 metadata 大小；`docker-daemon-resource-report.json` 从外层隔离 DinD 容器内按 process name 定位 `dockerd` 并持续读取其 `/proc/<pid>/status`，单独记录 daemon RSS 峰值。JSON 显式记录 `stream_hold_seconds` 与采样时点；普通 PR/package artifact 使用 1 秒 warm-up、5 秒 idle sample 和 1 秒 SSE hold，只用于快速检查进程存活、宽 RSS ceiling 与 StreamGate 释放，不作为正式基线。它们仍是“本地/CI 实测，不代表目标生产主机实测”。不得设置可能在恢复阶段杀进程的 `MemoryMax`。

## 本地基线

2026-08-29 在 Linux 6.6 WSL2、Node 24.12、基线 `b360d01e` 上，以 cgroup `cpu.max=200000 100000`、`memory.max=4294967296` 模拟 2 vCPU / 4 GiB。production embedded binary 经过 60 秒 warm-up 与 60 秒采样后：RSS 13,440 KiB，CPU 0.0167%，16 FD、5 tasks，binary 20,697,056 bytes。隔离 Registry+DinD 正式 60 秒 stream 场景中，in-process control-plane idle RSS 26,084 KiB，8 streams 在窗口末端 RSS 29,028 KiB（增量 2,944 KiB），峰值 30,124 KiB，metadata 2,434,439 bytes；独立 `dockerd` 峰值 80,688 KiB。此前同一最终代码的完整 DinD 5/5 也已通过。最终 commit 由 CI artifact 复核；测试进程包含 HTTP harness，所以它是偏保守回归基线，不冒充 production binary 的精确常驻值。

2026-08-30 在同一 WSL2 工具链上，M6 worktree（基于 `03206a4b`）的 production embedded binary 完成 60 秒 warm-up + 60 秒 idle sample：RSS 13,824 KiB、CPU 0.0000%、16 FD、15 tasks、binary 21,079,176 bytes。隔离 Registry+DinD 的正式 60 秒窗口保持 8 条 authenticated SSE，同时穿过 signed webhook、durable wake、private Registry polling 和既有部署/回滚链路：control-plane idle 29,248 KiB，窗口末端 30,528 KiB（增量 1,280 KiB），峰值 31,152 KiB，metadata 2,546,541 bytes。nonce replay TTL/批量清理、每 app 单 sequence 和 16 permit/120 global/10 per-app rate 上限均由定向测试验证；最终 commit 数值继续由 CI artifact 复核。本机未设置 2C4G cgroup ceiling，因此这些数字只作为宽回归基线。

两份报告共同验证 idle、streams、部署/回滚与受限 metadata 场景，但不等价于目标生产主机实测，也不会把 Docker 峰值混入 SoloDock RSS。目标主机上线前仍需在维护窗口按同一 workload 复测；`MemoryHigh=256M` 是 soft pressure signal，不是 kill limit，若目标宿主的 control-plane 常态或峰值显著高于 CI 基线再调整。
