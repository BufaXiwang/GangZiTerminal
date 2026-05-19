# Agent Harness Roadmap

> 长对话不崩的工程化路径。开题原因：一次 chat 投喂大量 tool result 后撞
> `hard_limit_tokens` 直接 hard fail，"开新对话"是兜底而不是修复。
>
> 参考：Claude Code（公开行为）/ Anthropic Cookbook 的 agentic patterns。

## 现状（commit dd75df4 完成）

三级压缩链 `MicroClear → Drop → HardLimit`，外加 Summarize 软兜底：

```
provider call
  ↓
context::compact_if_needed (规则化，同步)
  ├─ NoOp        (< soft_limit)
  ├─ MicroClear  (清白名单工具的旧 ToolResult)
  ├─ Drop        (整条丢老消息)
  └─ HardLimit   → return Err 立刻终止 run
       ↑ 这里之前没机会跑 Summarize
loop_::run_agent
  ├─ Summarize tier (异步，仅 chat 启用)
  │   触发：MicroClear 后 tokens > summarize_threshold
  │   流程：调便宜模型把 messages[..keep_from] 压成 6 段中文摘要
  └─ ...
```

**已修**（dd75df4）：
- Tool result 入口截断：text >6000 chars / JSON >8000 chars 自动加 marker
- 默认值：hard 160k→190k，soft 80k→130k，summarize 120k→150k
- Settings 暴露 AgentBudgetBlock：用户可调 hard/soft/turns/search/timeout

**未修**：以下是这份 roadmap 的实际范围。

---

## 待办（按 ROI 排序）

### P0 — Summarize 顺序倒置

**问题**：`compact_if_needed` 一旦判 `HardLimit` 就立刻 `Err` 返回（`loop_.rs:182-188`），Summarize 没机会跑。这是个**逻辑 bug**——本意是"Summarize 是 MicroClear 的软兜底"，但实际是"Summarize 仅在没撞 HardLimit 时才跑"。

**期望**：
```
compact_if_needed
  ├─ NoOp / MicroClear / Drop → 直接用
  └─ HardLimit
       ↓
   try Summarize (即使尾窗超 hard 也试一次)
       ├─ Ok    → use summary, retry compact_if_needed
       └─ Err   → 现在才 return hard fail
```

**关键改动**：
- `pipeline/agent/loop_.rs`：把 `if report.action == CompactAction::HardLimit { return Err(...) }` 块挪到 Summarize tier **之后**
- 但 Summarize 自己也可能超 hard（要送进去摘要的就是超长内容）——这是 OK 的，Summarize 调便宜模型自己有 200k context 接得住
- 失败路径需要清晰：summarize provider 错误 / 输出不合规 → 这时才 hard fail

**工作量**：~80 行 + 单元测试

---

### P1 — micro_clear 允许进入 keep_n 窗口

**问题**：`compact_if_needed` 把 messages 末尾 `keep_n * 2` 条作为不动区。当用户连续提问、agent 每轮跑 3-5 个工具，**最近 6 轮可能堆积 6×5=30 条 tool_result**，每条平均 3k token = 90k token。这部分本身就超 hard，micro_clear 守着不清。

**期望**：keep_n 不是硬地板，是软偏好：
- 默认场景 → 守 keep_n，保护因果链
- HardLimit 临界 → 退守到只保最后 1 轮（user + assistant 两条），其余 keep_n 区里的工具结果允许 stub

**关键改动**：
- `context::compact_if_needed` 在判 HardLimit 前**再跑一次 micro_clear，这次允许触及 keep_n 区**（但最后一对消息原样保留）
- 加 `CompactAction::MicroClearAggressive` 变体，让前端能看到"已动到最近窗口"
- 测试覆盖"最后 N 轮全是大工具结果"场景

**工作量**：~60 行 + 测试

---

### P2 — Agent 自调 `compact_now` 工具

**问题**：现在压缩是被动触发（撞到阈值才动）。Claude Code 的 `/compact` 思路是让 agent 主动喊："context 沉了，压一下"。

**期望**：
- 新工具 `compact_now(reason: string)` 注册到 chat / scan registry
- agent 看到 system prompt 提示"context tokens 当前 ~120k，可主动 `compact_now` 释放"
- system prompt 每轮注入 `[现在 context 大约 X token, soft=130k, hard=190k]`，让 agent 有自感知
- 工具本身就是触发 Summarize tier 的同一路径

**关键改动**：
- `adapters/agent_tools/`: 新 `compact.rs` 实现 `CompactNowTool`
- `pipeline/agent/prompt.rs`：system prompt 每轮带 token 余量
- `pipeline/agent/loop_.rs`：识别 `compact_now` 工具调用 → 直接走 Summarize 流程，不等阈值
- 工具返 `{ before, after, dropped }` 让 agent 看到效果

**工作量**：~150 行 + 测试

---

### P3 — 持久化 working memory（跨 chat session）

**问题**：现在 chat history 全在 SQLite，但每次新对话都从空 context 起步。重要事实（用户偏好、关注标的、风险纪律）每次都得 agent 重新发现。

**期望**：跨 session 的 "auto memory"（类似 Claude Code 的 `~/.claude/projects/.../memory/`）：
- 一张 `agent_memory(id, kind, body, weight, created_at, last_used_at)` 表
- kind: user_preference / focus_stock / risk_rule / external_resource
- chat run 启动时按 weight + last_used_at 选 top-N 注入 system prompt
- 新工具 `remember(kind, body)` / `forget(id)` 让 agent 自主写入

**关键差异 vs Heuristics**：Heuristics 是"投资规则"（启发式），Memory 是"个人事实"（你不喝白酒、你的预算上限是 5 万）。两者数据模型独立。

**关键改动**：
- v6 migration：`agent_memory` 表
- `infrastructure/agent/memory_repo.rs`
- `pipeline/agent/prompt.rs`：注入 top-N memory
- 三个工具：`remember` / `forget` / `list_memories`

**工作量**：~400 行 + 设计文档

---

### P4 — Tool result preview / 按需展开

**问题**：即便单条截断了，agent 经常需要"看一眼整体" → 当前模式下整体就在 messages 里浪费 token。

**期望**：大输出**落 SQLite** 而非 messages：
- 工具返回 `{ preview: "前 1000 字摘要", result_id: "xxx", size: 12500 }` 到 messages
- agent 决定要展开时调 `read_tool_result(result_id, range=[0, 3000])` 取详情
- result 在 SQLite 保留 24h 后清理

**关键差异 vs 截断**：截断是"丢中间"，preview 模式是"全留但按需读"。

**关键改动**：
- v6 migration：`tool_result_cache(result_id, payload, expires_at)` 表
- `pipeline/agent/loop_.rs`：execute_one_tool 后判 size，>10k 落 cache 改返 preview
- 新工具 `read_tool_result`
- TTL 清理跑在 scheduler

**工作量**：~250 行 + 测试

---

### P5 — Subagent 卸载

**问题**：重活（多步研究、批量扫描）每步都喂进主 context。

**期望**：派子 agent 跑重活，主 context 只见 summary：
- 新工具 `delegate(task: string, tools: ["search_news", "get_kline"]) -> Summary`
- 子 agent 独立 context budget，跑完返"任务报告"
- 主 context 只多了"调了 delegate，返回了 N 字总结"

**关键改动**：
- `pipeline/agent/subagent.rs`：spawn 一个 mini run_agent，独立 messages/budget
- `adapters/agent_tools/delegate.rs`：工具实现
- 子 agent 用专用 system prompt（不带主流程的 identity / heuristics）

**关键风险**：子 agent 自己也可能爆——递归式 budget 管理需要小心。

**工作量**：~600 行 + 设计文档；**ROI 最低，最后做**

---

## 设计原则（Claude Code 风格借鉴）

1. **工具结果不能撑爆上下文**——P4/P5 的核心
2. **摘要永远先于硬失败**——P0
3. **Agent 自感知 + 自主控制**——P2 的 `compact_now` + 当前 token 余量提示
4. **持久化重要事实**——P3 的 memory
5. **避免重复工作**——result_id 跨轮可引用（P4 的副产品）
6. **降级而非崩溃**——任何阶段都有可执行的"退一步"路径

## 度量

每个 P 完成后，加到 Agent Health 面板的可观测指标：
- `harness_compactions_today`：今日 MicroClear / Drop / Summarize / CompactNow 各自计数
- `harness_hardfails_today`：HardLimit 触发次数（应 → 0）
- `harness_avg_context_tokens`：平均每轮 context 大小（trend down 是好事）
- `harness_summarize_failures`：summarize 调用失败次数（持续 > 0 说明摘要 provider 配错）

---

## 推进顺序建议

```
P0 (1 天)  →  P1 (0.5 天)  →  P2 (1.5 天)  →  落地观察 1-2 周
                                              ↓
P3 (3 天) ←  P4 (2 天)  ←  根据实际命中率决定
                                              ↓
                                          P5 (5+ 天，最后做)
```

P0+P1+P2 是"立刻不崩"的最小集，3 天能搞定。之后看实际数据决定 P3/P4/P5 哪个更急。
