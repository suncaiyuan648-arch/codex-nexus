# Codex Nexus Collector 生命周期解耦方案

状态：实施规划已确认

日期：2026-08-19

## 1. 改造目标

当前核心问题是 Nexus UI、Tauri 进程与 Usage Collector 共用生命周期：UI 重启会停止 JSONL watcher、账号 observation 和 rate-limit observation，进而造成 observation gap、新 session 无法安全归属账号，以及 `source_incomplete` 阻塞 estimator。

目标架构是：UI 可以退出、重启或崩溃，但 Collector 的 durable 数据账本必须独立存在。

```text
Codex
 ├─ JSONL
 ├─ App Server
 └─ rateLimits
       ↓
nexus-collector
 ├─ account observer
 ├─ rollout watcher / catch-up scanner
 ├─ token parser
 ├─ quota sampler
 ├─ ownership binding
 ├─ accounting verifier
 └─ estimator input
       ↓
SQLite
 ├─ Nexus Desktop UI (React + Tauri)
 └─ Analytics / Estimator
```

## 2. 数据恢复原则

### 2.1 已绑定 source

JSONL 是 durable event log。已有绑定的 source 使用数据库 cursor 从 `last_offset` 继续读取到当前文件末尾，Collector 重启不能造成 Token 丢失。

### 2.2 离线期间的新 source

Catch-up Scanner 仍然解析完整的 session、thread、turn、model、reasoning、speed、Token 和 timestamp。若无法证明账号归属，则保存 Token，但将 source 标记为 `unresolved`，不进入 account-level estimator。

### 2.3 不可证明的账号归属

Collector 离线期间如果发生账号 A 使用、切换账号 B、账号 B 使用，而没有连续的 ownership evidence，系统不能凭“当前账号”把历史 source 全部归给 B。多账号模式下，宁可 `unresolved`，也不能猜测。

## 3. Durable 数据模型

### 3.1 `account_presence_intervals`

持续记录账号 ownership timeline：

```text
id, account_key, account_id, email,
started_at, ended_at, source, confidence,
collector_instance_id
```

来源包括 `account/read`、`account/updated` 和 connection ready。账号变化时关闭旧 interval 并创建新 interval。

### 3.2 `rollout_sources`

把 file cursor 从 parser 内部状态提升为 durable source registry：

```text
source_id, canonical_path, file_identity,
session_id, thread_id, account_key,
binding_status, binding_source, binding_confidence,
first_seen_at, first_activity_at, last_activity_at,
last_offset, last_size, last_mtime,
parser_version, created_at, updated_at
```

`binding_status`：`verified`、`inferred`、`unresolved`、`quarantined`。

`binding_source`：`realtime_account_observation`、`existing_thread_binding`、`durable_file_binding`、`single_account_mode`、`retroactive_activity`、`unresolved`。

已 `verified` 的历史绑定不能因普通账号切换而改变；只有显式人工修复可以修改，并且必须写审计。

### 3.3 Collector session / gap

每次 Collector 启动记录 `collector_sessions`。正常退出写入 `stopped_at`；只有 `started_at`、没有 `stopped_at` 的旧 session 在下次启动时按 crash 恢复。

`collector_gaps` 记录：

```text
start_at, end_at, duration_ms, reason
```

`reason` 至少包括 `app_restart`、`collector_upgrade`、`crash`、`os_sleep`、`machine_shutdown`、`unknown`。检测到 OS sleep 的 clock jump 也必须生成 gap。

### 3.4 Binding audit / source health

每次 binding 变更写入 `source_binding_audits`，包括 source、旧/新账号、reason、evidence、timestamp。新增 source health 和 account data health，分开表达 Collector/source 完整性与 estimator 质量。

## 4. Catch-up 与绑定流程

Collector 启动顺序：

```text
1. acquire singleton lock
2. open DB and run Collector-owned migrations
3. recover crashed collector session
4. account/read
5. load durable source registry
6. catch up known sources from durable cursor
7. scan new sources
8. start filesystem watcher
9. rateLimits/read and usage/read
10. start scheduler
11. mark ready
```

已知 source 按 cursor 增量读取；路径变化通过 session/thread 继承绑定；Collector 在线创建的新 source 使用实时账号 observation 绑定。离线产生的新 source 先完整解析并保留为 `unresolved`。

当 unresolved thread 后续重新增长且 Collector 在线、active account 明确时，允许 retroactive binding，把整个 thread 的历史 Token 安全绑定到该账号，并重跑 attribution、category aggregation、quota estimator 和 accounting audit。

Parser 和 catch-up 必须幂等：重复解析相同 JSONL 不能产生重复 turn/token，重复启动且文件未增长时必须得到 0 条新记录。

## 5. Gap-aware quota / estimator 规则

Token 可以从 JSONL catch-up 恢复；rate-limit 只有当前快照，无法恢复 gap 内的精确变化时刻。因此 quota interval 跨 gap 时必须标记质量：

```text
exact | bounded_gap | long_gap | unresolved
```

第一版建议：`<=30s` short，`30s~5min` medium，`>5min` long。但阈值只提供信号，是否进入 estimator 还要结合 gap 时长、Token category、source completeness、账号绑定、quota delta 和 external usage risk。

跨 gap 的 quota step 应保存 `observation_gap_ms` 与 `gap_token_count`；长 gap 或无法证明归属的 source 不得进入 estimator。Official usage 只用于 completeness detector，不得把差额按模型比例分摊。

账号数据状态至少区分 `COLLECTING`、`VERIFIED`、`SOURCE_INCOMPLETE`、`ACCOUNTING_INCONSISTENT`、`REBUILDING`；estimator 状态至少区分 `INSUFFICIENT_DATA`、`ESTIMATED`、`BLOCKED`，避免把所有原因都显示成“参数不足”。

## 6. 进程与存储边界

目标 Rust workspace：

```text
crates/nexus-core       # 纯业务逻辑、模型、DB、parser、attribution、estimator contracts
crates/nexus-collector  # 独立 binary：RPC、watcher、scanner、scheduler、SQLite writer、IPC
src-tauri               # 桌面窗口、托盘、UI commands、IPC proxy
src                     # React UI
```

Collector 不能作为 Tauri 的普通 child process 管理，否则 UI 退出仍会杀死 Collector。生产环境由 OS 管理：macOS 使用 LaunchAgent (`RunAtLoad` + `KeepAlive`)，Windows 第一版使用 Startup Task + 独立进程，暂不引入 Windows Service 的安装复杂度。

SQLite 使用 WAL、`busy_timeout` 和 foreign keys；Collector 是 usage 数据库唯一业务 Writer，UI 只读并通过 IPC 请求刷新、重建和状态。Schema migration 也只能由 Collector 执行。

第一版 IPC 使用本机 Unix Domain Socket；Windows 后续替换 Named Pipe。消息至少包括 `GET_STATUS`、`REFRESH_NOW`、`GET_ACCOUNT`、`GET_CATEGORY_USAGE`、`GET_DATA_HEALTH`、`REBUILD_ACCOUNT`；事件至少包括 usage invalidated、rate-limit updated、account updated、collector health、rebuild progress。

Collector heartbeat 写入 `collector_state`（instance、pid、started_at、heartbeat_at、version、status），让 UI 能显示 Running / Reconnecting，而不是把采集状态误认为页面状态。

## 7. 实施分工与顺序

本轮规划 2 个 coder，严格串行；每个任务完成后都由一个全新的 reviewer 审查。Reviewer 不通过时，原 coder 收到具体问题并修复；每次修复完成后再次唤起全新的 reviewer，不能复用上一轮 reviewer。

### Task A — Durable data plane / Collector core（Coder A）

范围：

- 把现有 usage DB、模型和 rollout parser 抽成可复用的 core 边界，保持现有行为兼容。
- 增加/迁移 `rollout_sources`、`account_presence_intervals`、`collector_sessions`、`collector_gaps`、`source_binding_audits` 及所需 health 字段。
- 持久化 cursor、file identity、binding status/source/confidence，支持 known source catch-up、路径迁移、new unresolved source。
- 实现 ownership interval、retroactive binding、verified binding 不可被普通切号覆盖。
- 实现 Collector startup recovery、graceful flush、crash/sleep gap 记录、singleton lock、parser/catch-up 幂等。
- 将 Collector 主循环抽成可独立运行的 `nexus-collector` 入口；不要求本任务一次完成所有 UI IPC。

完成标准：现有单元测试通过；已绑定 source 离线增长可完整追平；新未知 source Token 保留但 unresolved；出现后续在线证据时可安全 retroactive binding；重复 catch-up 无重复账本；Collector 重启不会重复计数。

### Task B — IPC / UI health / gap-aware integration（Coder B）

前置：Task A reviewer 通过。

范围：

- 建立 UI ↔ Collector IPC proxy，UI 的 refresh/status/rebuild 不再直接控制 Collector scheduler。
- 将 `collector_state`、source health、account data health、gap diagnostics 暴露到前端。
- 接入 gap-aware quota quality、`observation_gap_ms`、`gap_token_count` 与 estimator quality gate；unresolved source 必须被排除。
- 加入 macOS LaunchAgent 与 Windows Startup Task 的安装/开发配置和文档；Collector 不由 Tauri child process 托管。
- 增加 integration/reliability tests：UI 重启多次、Collector 重启多次、known source catch-up、offline unknown source、retroactive binding、多账号无证据不猜、OS sleep/gap、quota warning。
- 更新开发运行方式与用户可见状态文案。

完成标准：UI 重启不影响 Collector；Collector health 可观察；Token invariant 通过；gap 明确可见且不会悄悄进入 estimator；IPC 断线能显示状态而非误报为 UI disconnected；构建与测试通过。

## 8. 统一验收清单

- UI restart → 0 Token loss。
- Collector restart → 已绑定 source 0 Token loss。
- Collector offline + known source grows → full catch-up。
- Collector offline + new unknown source → data preserved, binding unresolved, estimator excluded。
- 后续在线证据出现 → source 可安全恢复并重算聚合。
- 账号归属有歧义 → never guess。
- Quota observation gap → explicitly represented with quality/confidence。
- Parser/catch-up/restart → idempotent，无重复账本。
- Collector 由 OS 生命周期管理，Tauri 退出不会杀死 Collector。
- `cargo test`、前端构建和相关集成测试通过。

最终原则：不追求 Collector 永远不离线；要保证任何离线都被记录，可恢复的数据全部恢复，无法证明的数据明确隔离，并且永远不会因为重启而悄悄制造错误归因。
