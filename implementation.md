有，但你现在已经从“能跑”跨进“可以当一个真正桌面工具维护”的阶段了。后续改造我建议分成 **必须做 / 很值得做 / 暂时别做** 三层，不要继续无脑堆架构。

你现在已经具备：

```text
单例 CodexRpcClient        ✅
单一 app-server           ✅
requestId 路由            ✅
pendingRequests           ✅
rateLimits 实时推送        ✅
自动断线重连              ✅
generation 隔离旧连接      ✅
指数退避                  ✅
重连后自动同步 snapshot    ✅
Windows Codex 路径探测     ✅
macOS 路径兼容             ✅
```

而且当前继续使用 `stdio + JSONL` 是对的。Codex App Server 官方把 stdio 定义为默认 transport；WebSocket transport 目前仍标记为 experimental / unsupported for production，所以这个监控器没必要为了“看起来先进”改成 WS。([OpenAI Developers][1])

## 第一优先级：把可靠性收尾

我下一步最建议做的是 **健康状态 + 手动重连 + 请求并发化**。

### 1. Snapshot 三个 RPC 改成并发

此前一次 Refresh 是：

```text
account/read
    ↓
rateLimits/read
    ↓
usage/read
```

也就是串行。

实际上这三个请求之间没有依赖，因此现在改为：

```text
             ┌─ account/read
Refresh ─────┼─ rateLimits/read
             └─ usage/read
```

你的 RPC Client 已经支持 request ID 和 pending map，所以底层已经具备并发能力。

这样一次 refresh 的耗时会从近似：

```text
Taccount
+
TrateLimit
+
Tusage
```

降到接近：

```text
max(
  Taccount,
  TrateLimit,
  Tusage
)
```

这也是你刚做 `pendingRequests` 真正开始产生价值的地方。

**已完成。** `get_codex_snapshot` 现在会先并发发出这三个请求，再统一等待结果；因此一次 Refresh 的耗时不再叠加三个 RPC 的响应时间。

---

### 2. 增加一个“手动重连”

现在断线只能自动等：

```text
1s → 2s → 4s → ...
```

UI 可以加：

```text
Codex disconnected

Retrying in 8s

[ Retry now ]
```

调用：

```text
reconnect_codex()
```

立即打断当前退避等待，主动重连。

**已完成。** Rust 端通过 `Condvar` 唤醒当前退避线程，`reconnect_codex()` 会立即打断等待并主动开始连接；UI 会显示倒计时和 `Retry now` 按钮。

---

### 3. 区分“连接问题”和“账号问题”

现在可能出现：

```text
app-server Ready
```

但是：

```text
account/read
→ 未登录
```

这不是：

```text
Disconnected
```

而应该显示：

```text
Codex connected
Account unavailable
```

建议状态分成两层：

```text
TransportState

Disconnected
Connecting
Ready
```

和：

```text
AccountState

Unknown
SignedIn
SignedOut
Error
```

因为官方还有：

```text
account/updated
```

notification，会在认证模式变化时发出，并在可用时包含 `planType`。([OpenAI Developers][1])

所以以后用户：

```text
退出 Codex
重新登录
切换账号
```

你的 Monitor 可以实时更新，而不是等 5 分钟。

**已完成。** Transport 状态仍由连接生命周期表示；Snapshot 另外返回 `accountState`（`unknown`、`signedIn`、`signedOut`、`error`）。`account/updated` notification 会转发到前端并立即触发账号刷新。

---

## 第二优先级：把它真正做成“监控器”

现在你的数据已经足够了，下一步应该更多投入产品体验，而不是 Rust 架构。

### 4. Tray 直接显示额度

这是我最建议做的产品功能。

Windows Tray tooltip：

```text
Codex Usage

Weekly 98%
2% remaining

Reset in 5d 3h
```

macOS 菜单栏最好直接：

```text
Codex 98%
```

点开：

```text
Weekly
███████████████████░ 98%
Reset Aug 18 12:04

Today
12.8M tokens

Open Dashboard
Quit
```

这样用户根本不用打开主窗口。

---

### 5. 阈值通知

增加设置：

```text
Notify me at

80%
90%
95%
100%
```

例如：

```text
Codex usage reached 90%

Weekly quota has 10% remaining.
Resets in 2d 4h.
```

这里一定要做 **去重**。

不能：

```text
98%
→ notification
98%
→ notification
98%
→ notification
```

应该保存：

```ts
lastNotifiedThreshold
```

例如：

```text
89%
↓
90% → 通知一次

91%
92%
93%
→ 不通知

达到 95%
→ 再通知一次
```

然后 reset 后清空。

---

### 6. 自动检测额度重置

你的数据有：

```text
resetsAt
```

官方定义就是 quota 窗口下一次重置的 Unix 秒时间戳。([OpenAI Developers][1])

所以可以：

```text
98%
Reset in 3h 24m
```

重置发生后：

```text
98%
↓
0%
```

通知：

```text
Codex quota reset

Your Weekly quota is available again.
```

这会很实用。

---

## 第三优先级：做本地历史数据

目前：

```text
account/usage/read
```

会返回 token summary 和 daily buckets，但官方明确说明这些字段和 `dailyUsageBuckets` 本身都可能是 `null`。([OpenAI Developers][1])

而且你不能保证未来永远给全部历史。

所以如果你真想把它做成漂亮监控器，我建议自己本地存 snapshot。

第一版不用数据库。

直接：

```text
AppData/
└── Codex Nexus/
    └── usage-history.json
```

例如：

```json
[
  {
    "timestamp": 1786500000,
    "limits": {
      "codex": {
        "usedPercent": 52
      }
    },
    "lifetimeTokens": 310000000
  }
]
```

每：

```text
5 min
```

或者 quota notification 时记录。

然后你就能画：

```text
过去24小时额度消耗

100 ┤
 90 ┤                       ╭──
 80 ┤                  ╭────╯
 70 ┤             ╭────╯
 60 ┤        ╭────╯
 50 ┤────────╯
    └────────────────────────
      8am   12pm   4pm   8pm
```

这个数据 Codex 当前接口本身并没有直接给你“额度百分比历史”，所以本地采样很有意义。

---

## SQLite 现在要不要上？

**暂时不用。**

你这个应用的数据规模：

```text
每天几百条 snapshot
```

哪怕：

```text
5 min 一次
```

一年才：

```text
288 × 365
≈ 10.5 万条
```

后面需要：

```text
查询
聚合
30d/90d 图表
多账号
```

再换 SQLite。

第一版 JSON 或简单本地 KV 已经足够。

---

## 第四优先级：增强协议兼容性

这个属于工程质量。

### 7. 正式支持多个 Rate Limit Bucket

官方明确：

```text
rateLimits
```

只是兼容的单 bucket view，

而：

```text
rateLimitsByLimitId
```

才是多 bucket view。([OpenAI Developers][1])

文档甚至示例了：

```text
codex
codex_other
```

同时存在。([OpenAI Developers][1])

你现在 normalize 已经朝这个方向做了。

下一步 UI 也应该真正支持：

```text
Codex

Weekly
98%


Codex Other

1 Hour
42%
```

而不是假设永远只有：

```text
limitId = codex
```

---

### 8. 支持 earned rate-limit reset credits

你这次真实账号：

```json
"availableCount": 0
```

所以暂时没东西。

但官方现在已经支持：

```text
rateLimitResetCredits
```

并提供：

```text
account/rateLimitResetCredit/consume
```

来使用 earned reset；文档还要求用 `idempotencyKey`，并在成功后重新读取 rate limits。([OpenAI Developers][1])

未来如果你的账号返回：

```text
availableCount = 1
```

UI 可以出现：

```text
Weekly quota
98%

1 reset available

[ Reset quota ]
```

不过这个属于 **V2 功能**。

因为它已经从“监控”变成“执行账户操作”，我建议后面再加。

---

## 第五优先级：启动与安装体验

如果你最终不是只自己用，这部分必须做。

### 9. 开机启动

设置里：

```text
☑ Launch at startup

☑ Start minimized

☑ Close window to tray
```

桌面 Monitor 很适合：

```text
系统启动
↓
后台运行
↓
Tray 常驻
```

而不是每次手动开。

---

### 10. Close ≠ Quit

推荐：

```text
窗口 X
↓
hide window
↓
Tray 继续运行
```

真正退出：

```text
Tray
↓
Quit
```

否则用户一关窗口：

```text
CodexRpcClient
↓
挂了
↓
没有监控
```

那 Tray 工具就没意义了。

---

### 11. 打包

最终：

Windows：

```text
CodexNexus_0.1.0_x64-setup.exe
```

macOS：

```text
CodexNexus_0.1.0_universal.dmg
```

之后再处理：

```text
Windows code signing
macOS notarization
```

如果只是自己用，暂时不用签名。

---

# 第六优先级：代码结构收尾

现在 `codex.rs` 已经开始比较长了。

但我不会现在立刻让你继续拆。

等我们再增加：

```text
Account listener
manual reconnect
history
settings
```

再拆成：

```text
src-tauri/src/codex/
│
├── mod.rs
│
├── client.rs
│   └── CodexRpcClient
│
├── process.rs
│   ├── resolve_codex
│   └── spawn_app_server
│
├── protocol.rs
│   ├── request
│   ├── notify
│   └── pending
│
└── connection.rs
    ├── ConnectionState
    └── reconnect
```

现在马上拆只会让文件变多，没有真正收益。

---

# 有几件事我建议现在明确“不做”

**不要做自己的登录系统。**

复用：

```text
本机 Codex 登录
```

最好。

**不要自己保存 ChatGPT Token。**

完全没必要。

**不要自己部署后端服务器。**

这个应用天然：

```text
local-first
```

最好。

**不要换 WebSocket。**

官方当前 App Server 的 WebSocket transport 仍然是 experimental，而且文档明确说 unsupported for production workloads；本地桌面集成继续 stdio 最合适。([OpenAI Developers][1])

**不要上 Redux。**

现在：

```text
snapshot
connection
settings
```

这点状态 `useState/useMemo` 足够。

后面 settings/history 多了再考虑 Zustand。

---

# 我建议我们的下一步顺序

如果是我继续带你做，我会按：

```text
当前
  ↓
① Snapshot 三 RPC 并发化 ✅
  ↓
② account/updated 实时账号变化 ✅
  ↓
③ Tray 显示当前额度
  ↓
④ 80/90/95/100% 系统通知
  ↓
⑤ reset countdown + reset notification
  ↓
⑥ Close to Tray / Startup
  ↓
⑦ 本地历史
  ↓
⑧ Usage 图表
  ↓
⑨ Settings
  ↓
⑩ 打包 Windows / macOS
```

其中 **①–⑥ 做完，我就认为这个工具已经是一个完整可长期使用的 V1**。

后面的：

```text
历史趋势
多账号
reset credit
自动更新
```

都可以算 V1.5/V2。

Snapshot 三 RPC 并发化和 `account/updated` 实时账号变化已经完成；现在 V1 的监控、通知、托盘和本地历史闭环也已经接上。

## 当前实现状态与后续计划

### 已完成

1. **阈值通知**：设置页提供 80%、90%、95%、100% 选项；每个 `limitId + primary/secondary` 独立保存 `lastNotifiedThreshold`，同一阈值重复刷新不会重复通知。
2. **Quota reset**：根据 `resetsAt` 检测窗口是否进入新的 reset 周期；reset 通知只发一次，并清空该窗口的阈值去重状态。
3. **本地历史**：SQLite 已成为主数据源；旧的 `usage-history.json` 保留为兼容文件，不再作为分析图表的主来源。
4. **多 bucket**：UI、托盘和历史均按 `rateLimitsByLimitId` 读取，不再假设只有 `codex` 一个 bucket。
5. **启动与托盘**：支持 Launch at startup、Start minimized、Close window to tray；真正退出从托盘菜单执行。
6. **打包基础**：Tauri bundle 已启用，产品名和 NSIS 图标配置已准备好；签名、公证和 universal 构建仍属于发布阶段工作。

### 推荐交付顺序

1. 用真实账号跑一轮 5 分钟采样，确认 AppData 文件内容和系统通知权限。
2. 手动验证 89% → 90% → 98% → reset 后 0% 的去重与 reset 行为。
3. 在 Windows/macOS 各做一次 debug bundle，确认托盘、关闭窗口和开机启动。
4. 继续扩展历史导出、按模型费率计算 weighted usage，以及 quota cycle 分析。
5. 最后实现 earned reset credit 消费：调用带 `idempotencyKey` 的 RPC，成功后重新读取 rate limits；这项属于 V2 的账户操作。

### 明确不做

继续复用本机 Codex 登录和 stdio App Server，不增加自有登录、ChatGPT Token 存储、后端服务器、WebSocket transport 或 Redux。多账号切换、自动更新、签名/公证也不放进当前 V1。

## Usage Analyzer 实现说明

当前用量系统已经从单一 JSON 快照升级为 SQLite 本地分析层，数据库位于：

```text
Tauri AppData/
└── Codex Usage Monitor/
    └── usage.db
```

核心表：

```text
account_daily_usage  官方账户每日总量、lifetime token
turn_usage           本机 rollout 的 Turn token 明细
rate_limit_samples   usedPercent / resetsAt 历史采样
rollout_cursors      JSONL 文件 byte offset 与累计 token 状态
```

`account/usage/read` 的每日 bucket 会做 UPSERT；`account/rateLimits/read`、定时刷新和 `account/rateLimits/updated` 会写入额度采样。rollout collector 每 15 秒增量读取 `~/.codex/sessions` 和 `~/.codex/archived_sessions`，只保存 token、模型、effort、speed 状态等元数据，不保存 prompt、response 或 reasoning 内容。

Token telemetry 使用 session cumulative total 与 cursor 中保存的上一状态计算 Turn 增量，因此应用重启、重复事件和重复扫描不会再次累计。当前 rollout 如果没有可靠 service tier，会保存为 `Unknown`；配置了 priority/fast 只标记为 `Fast requested`，不伪装成已确认的有效速度。

历史分析支持 `7D / 15D / 30D / 90D / ALL`，以及 Model、Reasoning、Speed、Token Type 四种分类。官方账户总量与本机可归因 Turn 不一致时，差额显示为 `Unattributed`，不会强行分配给某个模型。

剩余 Token 不是官方字段。只有当 SQLite 中积累至少两个有效的“本地 Token 增量 / usedPercent 增量”区间时，界面才显示 `Estimated Remaining`；该数字始终带有估算说明，数据不足时不显示。

目前仍未实现：7/30/90 天导出、按模型费率计算 weighted usage、按模型分别估算剩余量、quota_cycles 周期表，以及 earned reset credit 的消费 RPC。这些功能建立在当前 SQLite 数据基础上继续扩展。

[1]: https://developers.openai.com/codex/app-server "Codex App Server | ChatGPT Learn"
