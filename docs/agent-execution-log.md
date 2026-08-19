# Agent 实施记录

本文件记录本方案的 coder / reviewer 执行结果。每个计划任务必须使用全新的 coder；每一轮 review 必须使用全新的 reviewer。

| 任务 | Coder | Reviewer 轮次 | 状态 | 结果 |
| --- | --- | --- | --- | --- |
| Task A — Durable data plane / Collector core | 待唤起 | 待唤起 | 待开始 | — |
| Task B — IPC / UI health / gap-aware integration | 待唤起 | 待唤起 | 待开始 | 依赖 Task A 通过 |

## Review 规则

1. Reviewer 必须检查实现、测试、diff 与验收标准，不只看 coder 的自述。
2. 不通过时，将 reviewer 的具体 findings 原样归纳后发回原 coder 修复。
3. 每次修复完成后唤起新的 reviewer；不得复用之前的 reviewer。
4. Task B 只有在 Task A 的 reviewer 明确通过后才能启动。
