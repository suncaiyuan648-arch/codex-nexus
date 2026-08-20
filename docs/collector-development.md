# Collector 独立运行与 IPC 开发说明

Task B 建立了 UI 与 Collector 的协议边界。UI 通过 Tauri proxy 请求本机
Collector endpoint；Tauri command 不打开可写 usage DB，也不直接调用 scheduler。
macOS/Linux 使用 newline-delimited JSON over Unix Domain Socket，默认 socket 是
usage DB 旁的 `collector.sock`。Windows 保留同一请求模型，第一版 transport 名称和
安装入口是 Named Pipe，后续 runner 接入时不需要改变前端 API。

协议请求：`GET_STATUS`、`REFRESH_NOW`、`GET_ACCOUNT`、`GET_CATEGORY_USAGE`、
`GET_DATA_HEALTH`、`REBUILD_ACCOUNT`。事件等价机制使用 `collector://` 事件：
`usage-invalidated`、`rate-limit-updated`、`account-updated`、`health`、
`rebuild-progress`。兼容的 Codex 状态/快照代理使用 `GET_CODEX_STATUS`、
`GET_SNAPSHOT`、`RECONNECT_CODEX`。旧的 `codex://` 事件仍保留作兼容；独立进程
没有 Tauri event loop 时，UI 以 health/snapshot polling 作为等价机制。

## 本地开发

当前版本的 collector service boundary 由独立 `nexus-collector` binary 运行，Tauri
启动时只发现 endpoint 并做代理，不会启动 scheduler、watcher 或 writer：

1. Terminal 1：`cargo run --manifest-path src-tauri/Cargo.toml --bin nexus-collector -- --database "<绝对路径>/usage.db"`
2. Terminal 2：仓库根目录执行 `pnpm dev`（前端开发服务器；运行完整 Tauri UI 时仍使用项目既有 Tauri dev 命令）。

生产部署时，先构建并安装独立的 `nexus-collector`，再启动 Tauri UI。UI 退出不会
停止 watcher、Codex app-server RPC 子进程、writer token/OS lock 或 heartbeat。Collector
是 usage DB 唯一业务 writer，UI 查询保持只读；独立进程重启会通过 lock/socket stale
recovery 重新接管。

## 健康与故障语义

`collector_state.heartbeat_at` 是 Collector 状态来源：15 秒内是 `Running`，过期是
`Reconnecting`，没有状态记录或 endpoint 无法连接是 `Unavailable`。这与 Codex RPC
页面连接分开显示，不能用页面刷新失败代替 Collector 断线。

`GET_DATA_HEALTH` 返回 source binding/lag、unresolved source、account data health、
collector gaps 和最新 rate-limit samples。unresolved source 的 token 可以保留，
但不进入 account estimator。quota interval 会记录 `observation_gap_ms`、
`gap_token_count` 和 `sample_quality`；短 gap 降低 confidence，long/unresolved gap
阻断估算。Official/local 差额不按模型比例分摊。

## 安装生命周期模板

- macOS：安装器必须把 `packaging/macos/com.codex.nexus.collector.plist` 中的四个
  `__NEXUS_*_ABSOLUTE_PATH__` placeholder 替换为绝对路径后，再复制到
  `~/Library/LaunchAgents/` 并执行 `launchctl bootstrap gui/$UID ...`。模板不使用
  launchd 不会展开的 `~`，包含 `RunAtLoad` 与 `KeepAlive`。
- Windows：当前 standalone binary 会 fail-closed，因为 Named Pipe 尚未实现；
  `packaging/windows/startup-collector.ps1` 会明确报错并退出，不会安装一个看似可运行
  但实际不可连接的 Startup Task。Windows 第一版的可执行路径是完成 Named Pipe
  transport 后，用该脚本创建 `schtasks /Create ... /SC ONLOGON`；当前不会伪装支持。

`cargo run --bin nexus-collector -- --once` 只执行一次 RPC/JSONL refresh 后退出，适合
做构建 smoke test；常驻模式才会打开 endpoint、启动 watcher 并保持 heartbeat。
