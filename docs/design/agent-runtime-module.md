# Agent Runtime Spec（精简版 v4）

> 本文档定义 Agent 在本产品里的**业务运行期**：什么时候唤起 Agent、每类运行用哪些工具、如何联合 Quotes / News / Account、如何把模型判断沉淀成**可审计、可复盘、可测**的决策链。
>
> Agent 的底层模型渠道、消息、工具注册协议、context compaction、canonical loop、fork 子 agent、skill 执行机制见 [agent-infra-module.md](agent-infra-module.md)。
>
> 模块级领域契约以 `docs/design/*-module.md` 为准；共享类型见 [shared-types.md](shared-types.md)。

> 🟢 **两处设计决策**：
> 1. **证据不单独持久化**——靠 `run_id → ToolCall 审计`（Infra 已记每次 `fetch_*` 的完整 in/out）按需重建。约束：复盘报告引用的 run 的 payload **不被 GC 清理**（pin），见 §8。
> 2. **策略保持纯自然语言**——`InvestmentStrategy` 当前只有一段自然语言 `strategy`，**不做结构化硬约束**；后续如需可再加字段（如仓位/止损/集中度限额）。本阶段硬约束只有**账户级**（`AccountRiskPolicy`，Account fail-closed）一道机器闸门；**不做编排级风控闸门**（熔断/追高/额度拦截）——模拟账户，亏损本身就是学习闭环的反馈信号，风险偏好由自然语言策略供 LLM 自律。
>
> 🔗 **跨 BC 协同（实现前需对齐 Account spec）**：本版恢复 `account-triggered` 实时消费（§6），与 [account-module.md](account-module.md) 的 `AccountTrigger` / `mark_trigger_handled` 契约一致；另需 Account 的 `operate_account` 接受 `clientOrderId` 幂等键去重（§3 AgentTrade 崩溃恢复）。

---

## 一句话定位

**Agent Runtime 是产品里的 Agent 应用层。** 它把"市场发生了什么"翻译成"Agent 在什么时候、用哪些工具、基于哪个策略，做出并记录了什么投资判断"，并把结果沉淀成一条可按 `run_id` 串起来的决策链。

它服务的**最高契约 = 学习闭环**：

```text
Agent 自己决策 → 自己 operate_account 下单 → 自己复盘
  → 读模拟账户的真实结果（成交 / PnL / 与基准的超额）反推「策略合不合理」
  → 形成策略建议（是否调整，由用户在对话中确认后才改）
模拟盘 = 这个闭环的「真实反馈源」。不连真券商。
```

分工边界：

```text
News / Quotes / Account 只表达「发生了什么」（事实 + 事件 + 只读 facade）
Agent Infra          负责「把一次 LLM loop 稳定跑完」（消息 / 工具 / 压缩 / fork / skill）
Agent Runtime        负责「何时唤起谁、注入哪些工具与策略、把结果变成可复盘的决策链」
```

契约强度：

- `AgentRun`、trigger 集、`InvestmentStrategy`、`AnalysisResult`、`AgentTrade`、review 只读 sub-agent、闭环关联键 `run_id` 是 `Spec-as-source`。
- buffer 阈值（M / N / 4h）、各调度 cadence、退避参数、settings 缺省、文件组织是 `Spec-anchored`。
- 策略结构化硬约束、自动策略晋升、自动调参、回测引擎、多 agent 协作**不在本阶段**。

---

## 1. 责任边界

Agent Runtime 负责：

- 启动并管理后台 loop（news buffer 消费、收盘复盘定时、账户 trigger 路由、行情/账户维护调度）。
- 监听应用内事件（`news-refreshed`、`account-triggered`），路由成 `AgentRun`；账户重建由自驱 quote tick 触发（§6），不消费 universe `market-quotes-refreshed`。
- 为每类 run 选择 **mode**、注入 allowed tools、构造实时上下文、注入当前 active `InvestmentStrategy`。
- **防自我打架信息注入**：会下单 run 的 L3 强制注入「当日已下单意图 + 活跃挂单 + 持仓 + 剩余可下单额度」——信息层提示，无强拦截（机器闸门只有 Account fail-closed）。
- 调用 Agent Infra 跑 canonical loop。
- 记录 `AnalysisResult`、`AgentTrade`；复盘产物（review sub-agent）写成 workspace 文件。
- 维护 news 待分析 buffer、`orderId → runId` 反查索引、in-flight lock、事件消费幂等、heartbeat、启动恢复。
- 向前端 emit 运行状态、分析结果、run 起止。

Agent Runtime 不负责：

- Provider wire format 适配、context 压缩细节、fork / skill 执行机制（全在 Infra）。
- 行情 provider / 指标 / universe；新闻 provider / 正文抽取 / 去重；账户现金 / 持仓 / 成交模拟 / 保护触发判定 / 账户级 fail-closed 风控（各 BC 自管）。
- 绕过 Agent 做交易决策；绕过 Account 直接写订单 / 持仓 / 现金；真实券商交易。

边界规则：

- Runtime 位于 `pipeline/`，可调用各 BC 公开 use case / facade，但**不属于任一 BC**。
- Quotes / News / Account **不知道 Runtime 存在**；任一层不得 import Agent 代码。
- 风控**只有一层机器闸门**：账户级硬约束（`AccountRiskPolicy`：单票 / 总仓 / 单笔 / 日新单等，Account fail-closed 兜底）。**Runtime 不设编排级风控闸门**（无熔断 / 追高 / 额度拦截）——模拟账户，策略好坏由复盘闭环反馈。自然语言策略表达风险偏好供 LLM 自律，不承载机器校验；Runtime 只做防自我打架的**信息注入**（L3，§6）。

---

## 2. 核心闭环（决策链 = run_id 串起来的脊梁）

```text
┌─ 主动/响应 trigger ─────────────────────────────────────────────┐
│ 对话(用户消息) │ news(buffer M/N) │ account_trigger(止损/成交…实时) │
└────────┬───────────────┬───────────────────┬────────────────────┘
         └───────────────┴─────────┬─────────┘
                                   ▼
         AgentRun(run_id, mode, strategyVersion 冻结)
         注入 = L1 角色 + L2 策略(自然语言) + L3 实时上下文(含当日已下单意图)
                                   │
      ┌────────────────────────────┼─────────────────────────────┐
      │(news)                      │(dialogue/news/account_trigger)│
      ▼                            ▼                               │
 AnalysisResult            operate_account                          │
 {action|no_action}        (clientOrderId 幂等) → AgentTrade        │
 →右侧列表                  {runId,strategyVersion,reason,result}    │
                                   │                                │
                                   ▼                                │
                  Account 受理/成交/拒绝 + 可能 emit account-triggered │
                                   │ (止损/成交 → 实时回到 account_trigger)
                                   ▼
                          orderId → runId 反查索引
                                   │
   ┌── review 只读 sub-agent（不下单）────────────────────────────┐
   │  · 收盘调度/手动起顶层 review run：读决策链+账户+基准 → Runtime 写报告 │
   │  · 对话/news run 经 run_subagent(只读) 临时复盘：返回结论文本   │
   └──────────────────────────┬──────────────────────────────────┘
                              ▼
            复盘报告(workspace 文件) → 左侧列表
            含：交易清单 + 按策略版本归因 + vs 基准超额 +
                样本量与置信度声明 + 上次建议 follow-up + 策略评估
                              │
                              ▼
            策略更新（★只在对话中、用户确认后★）→ InvestmentStrategy version+1 → 下次 run 注入
```

- **`run_id` 是全链关联键**；给定任一 `run_id` 可回溯：看了什么（ToolCall）→ 判断（AnalysisResult）→ 下单（AgentTrade）→ 账户结果 → 触发响应 → 复盘。
- **策略版本贯穿**：run 创建时**冻结** active 版本进 `AgentRun.strategyVersion`，全程只用此快照。

---

## 3. 领域模型

### 概念全集（3 + AgentRun + 报告文件）

| 概念 | 身份 | 作用 |
|---|---|---|
| `AgentRun` | `runId` | 一次 Agent 运行；决策链主键 |
| `InvestmentStrategy` | `strategyId` + `version` | 投资策略（一段自然语言）；版本化；**只在对话中用户确认后写** |
| `AnalysisResult` | `resultId` | news 分析产物（action / no_action）；前端右侧列表 |
| `AgentTrade` | `tradeId` | 每次 `operate_account` 的审计戳（runtime 侧，不耦合 Account） |
| `ReviewSuggestion` | `suggestionId` | review run 登记的策略建议；供下次复盘 follow-up 确定性对账（建议↔upsert↔版本） |
| 复盘报告 | workspace 文件 | review sub-agent 产物；**不入库**；前端左侧文件列表 |

### `AgentRun`

```ts
type AgentRunMode = "dialogue" | "news" | "account_trigger" | "review";

type AgentRunTrigger =
  | { kind: "user_chat"; messageId: string }
  | { kind: "news_batch"; newsIds: string[] }
  | { kind: "account_trigger"; triggerId: string }
  | { kind: "eod_review"; tradeDate: TradeDate };   // 收盘调度起 review sub-agent

type AgentRun = {
  runId: string;
  mode: AgentRunMode;
  trigger: AgentRunTrigger;
  parentRunId?: string;              // review 被对话/news fork 时，指向父 run
  provider: string;
  wireFormat: "messages" | "responses" | "chat_completions";
  model: string;
  strategyVersion?: number;          // run 创建时冻结的 active 策略版本
  causationRunId?: string;           // account_trigger run 关联到建仓 run（经 orderId→runId 反查）
  status: "queued" | "running" | "completed" | "failed" | "cancelled";
  startedAt?: OccurredAt;
  endedAt?: OccurredAt;
  error?: string;
};
```

规则：

- `runId` 是事件、工具调用、模型 turn、AnalysisResult、AgentTrade 的关联键。
- **策略版本冻结**：run 创建时把 active `version` 写入 `AgentRun.strategyVersion`，全程不变（注入 L2、AgentTrade 盖戳都用它），不因并发 `upsert_investment_strategy` 中途改变 → 保证确定性 + 归因正确。
- `account_trigger` run 经 `orderId → runId` 索引把响应关联到原始建仓 run（写 `causationRunId`）；查不到记 `mapping_missing`。
- **stop_reason → status 映射**：`completed`/`provider_stop` → `completed`；`cancelled` → `cancelled`；**`max_turns` → `failed`**（跑满轮数 ≈ 任务未收口，不得让调用方据 Completed 误标 trigger handled / news analyzed）；其余（`token_budget_exceeded`/`context_limit`/`error`…）→ `failed` + error 记 stop_reason。
- `review` run 可由**收盘调度**起（顶层 run，`trigger=eod_review`），也可被对话/news run **fork**（`parentRunId` 指向父 run）；两种都**只读、不下单**。
- 单次 run 用单个 provider channel / model；失败也落 `status`+`error`；无需唤起时不创建 run，只在事件消费记录标 `ignored`。

### mode 与工具（review 只读；其余可下单）

| mode | 起因 | 上下文（L3） | 领域工具 | operate_account |
|---|---|---|---|---|
| **dialogue** | 用户消息 | 独立连续对话线程 + 策略 + 账户/行情按需 + 当日已下单意图 | 全部读 + `update_watchlist` + `operate_account` + `upsert_investment_strategy` + `run_subagent` | ✓ |
| **news** | buffer M/N（§5） | 隔离单次：本批 news + 策略 + 账户/行情 + 当日已下单意图 | `fetch_news/quotes/account` + `update_watchlist` + `operate_account` + `record_analysis` + `run_subagent` | ✓ |
| **account_trigger** | `account-triggered`（实时） | 隔离单次：本 trigger + 原始建仓 run 摘要 + 账户/行情 + 当日意图 | `fetch_account/quotes/news` + `operate_account` | ✓ |
| **review** | 收盘调度 / 手动触发 | 隔离单次：scope 内决策链(AnalysisResults+AgentTrades) + 账户结果 + 组合 vs 基准超额(Runtime 算) + 上次建议 follow-up(Runtime 对账) + 策略 | `fetch_account/quotes/news` + `record_review_suggestion`（只读，不下单；结论文本回 + 报告由 Runtime 落盘） | **✗ 永不下单** |

- **review 永不 `operate_account`**——它是**只读复盘**：读决策链 + 账户结果 + 基准，产出结论。收盘 review 由 Runtime 把结论确定性落盘成报告（不依赖 agent 写文件），避免"按当天结果做事后补救"污染归因。
- **复盘两条路径**：① **顶层 review-mode run**（收盘调度 / 手动 `run_review` 命令触发），跑只读 review agent 并由 Runtime 写当日报告；② **对话/news 中临时复盘**——直接用 `run_subagent` fork 一个隔离子 agent、`allowedTools` 收紧为只读 `fetch_*`，拿回结论文本辅助判断（不另设 `run_review` 工具：run_subagent 已覆盖，只读由 allowedTools 保证）。
- 其余三 mode 都能 `operate_account`；唯一机器闸门是 Account 级 `AccountRiskPolicy` fail-closed（Runtime 不拦截）。
- 会下单的 run（dialogue/news/account_trigger）的 L3 **强制注入**「当日已下 AgentTrades + 活跃挂单 + 持仓 + 剩余可下单额度」（防自我打架，§6）。
- 工具集是 mode 固定属性；领域**写**只走结构化工具，绝不经 `run_bash`；本地工具 / fork / skill 机制见 [agent-infra-module.md](agent-infra-module.md)（fork 无嵌套：子 agent 内剔除全部 spawn 类工具 `{run_subagent, run_skill}`，Infra 按 spawn-class 标记剔除，见 agent-infra §3.5）。

### 分层 system prompt（L1 / L2 / L3）

- **L1 · 角色与纪律（所有 mode 共享、固定）**：你是 A 股模拟交易投资者，自驱动；不确定不交易；行情过期不下单；**消息驱动交易须先判断是否已 price-in**；下单前形成清晰理由；保守审慎。review mode 额外：客观复盘、不美化、区分运气与能力。
- **L1 · 自主工作流（agentic loop，所有可下单 mode 共享）**：agent 按 **理解意图 → 拆解 → 取数(工具) → 观察 → 校验 → 收口** 自驱动多步完成任务，不在中途停下等用户：
  - **不空承诺**：禁止输出「我来查一下 / 请稍等」之后就停止；需要数据就**在同一条回复里直接发起工具调用**，拿到 `<tool_result>` 再下结论。
  - **该拆就拆**：对明显多步的任务（如「这只票要不要买」≈ 行情 + 基本面 + 持仓 + 资讯 + 策略对照），先用一两句列出要走的步骤（可用 `todo_write` 登记清单并随进度更新），然后**连续执行直到任务真正完成**，而不是只做第一步就停；无依赖的工具一次回复内并发调，不分多轮。
  - **取数后才分析**：没有工具返回数据不得给任何投资结论（保留既有纪律）。
  - **收口前自检**：给最终结论前自检「用户目标是否已被完整满足、关键数据是否齐」；缺了就继续取，而不是提前宣布完成。
  - 复杂只读深挖（如临时复盘历史决策）用 `run_subagent` fork 只读子 agent 拿回结论，不污染主线。
  - **信息获取分层**：本地资讯 `fetch_news`、开放互联网 `web_search`（搜）+ `web_extract`（读正文）。**联网深度调研**（公司产业链 / 上下游 / 竞争格局，或多角度搜+读+综合）用 `run_subagent` fork 子 agent 去跑（子 agent 用 web_search/web_extract），只回**整理好的简报**，避免原始搜索结果污染主对话；简单查证才直接 `web_search`。
- **L2 · 投资策略（隔离层）**：注入当前 active `InvestmentStrategy.strategy`（自然语言全文）。唯一策略来源，可整段替换、版本化。
- **L3 · 实时上下文**：见上表；会下单的 run 必含「当日已下单意图」；review 含 scope 内决策链 + 基准。
- **自主 run（news / account_trigger / review）注入一条 user-role 任务指令消息驱动本轮**：这三类 run 的 L1/L2/L3 全在 system prompt，若 `input` 为空则发给 messages-format provider 的 `messages` 数组为空（400）。故为每类自主 run 注入一条 user-role `AgentMessage`，内容是该 mode 的任务指令（让 agent 知道该干嘛并触发工具使用）。dialogue 本就带用户消息，不受影响。

### `InvestmentStrategy`（纯自然语言）

```ts
type InvestmentStrategy = {
  strategyId: string;
  version: number;
  strategy: string;          // 自然语言：投资理念 / 选股 / 风控 / 仓位 / 止盈止损纪律，一段说清
  status: "active" | "paused";
  createdAt: OccurredAt;
  updatedAt: OccurredAt;
};
```

规则：

- `strategy` 是**自然语言**（本阶段不结构化），注入 L2 给所有 mode 共用。**后续若需机器可校验的硬约束，可在此 type 上加结构化字段（如仓位/止损/集中度限额）**——届时回写本 spec。
- 当前硬风控由账户级 `AccountRiskPolicy`（Account fail-closed）兜底（不靠策略结构化）；无编排级闸门。
- **单点写、用户确认**：策略只能在**对话 mode**、用户明确确认后经 `upsert_investment_strategy` 写新版本。news / account_trigger / review run **不写策略**；复盘只给建议。
- 对话中用户强调新偏好/约束 → Agent **确认是否写进策略文本** → 确认才 version+1。
- 每次更新 `version+1`，旧版本保留可追溯；历史版本可经 `fetch_investment_strategy({includeHistory:true})` 读出（前端展示历史版本，见 §9）。
- 首启若无 active 策略，Runtime seed 一条内置 baseline（含基础纪律：仓位上限 / freshness / 止损 / 不确定不交易）。无策略时 L2 为空，自动下单 mode 默认禁写交易。
- 本阶段不做策略晋升 / shadow run / 自动调参 / 回测。

### `AnalysisResult`（news 分析产物 → 右侧列表）

```ts
type AnalysisResult = {
  resultId: string;
  runId: string;
  kind: "action" | "no_action";
  summary: string;                        // 结论 + 理由（含「为什么现在进还来得及/已 price-in」判断）
  relatedCodes: TsCode[];
  tradeIds?: string[];                    // 若 action 且下单，关联 AgentTrade
  createdAt: OccurredAt;
};
```

规则：

- **产生机制**：由 news run 的 agent 经 `record_analysis` 工具声明 `kind/summary/relatedCodes`；`tradeIds` 由 Runtime 按 `run_id` 关联本 run 的 `AgentTrades`；Runtime 持久化并 emit `agent-analysis-result`。
- **summary 首行 = 主题标题**（≤30 字，概括本批新闻主题 + 判断要点；不以「结论」「no_action」开头）——前端列表用首行做条目标题；正文（结论 + 理由）从第二行起。
- news mode 分析完一批 news 后 emit；右侧列表按 `createdAt` 倒序。
- `no_action` 也要 emit；**大多数 news 应是 no_action**。
- action 类 `summary` 必须说明「为什么现在进还来得及」（边际信息），否则倾向降级 no_action（price-in 判断纪律，L1）。
- 证据不另存，按 `run_id` 拉 `fetch_news` ToolCall。

### `AgentTrade`（每次 operate_account 的审计戳）

```ts
type AgentTrade = {
  tradeId: string;
  runId: string;
  clientOrderId: string;                  // 幂等键：下单前生成，传给 Account 去重 + 恢复对账
  strategyVersion?: number;               // 来自 AgentRun.strategyVersion（冻结值）
  reason: string;
  accountInputSummary: string;
  status: "submitting" | "settled";       // submitting=调用前已落库；settled=拿到 Account 结果
  accountResultRef?: AccountResultRef;
  createdAt: OccurredAt;
  updatedAt: OccurredAt;
};

type AccountResultRef = {
  accepted: boolean;
  orderId?: string; fillIds?: string[]; positionId?: string;
  accountEventIds?: string[]; rejectionEventId?: string;
  reason?: ErrorCode; message?: string;
};
```

规则：

- **每次 `operate_account` 写一条 `AgentTrade`**（接受/拒绝都记）。
- **崩溃恢复**：调 Account **前**先落 `AgentTrade{status:"submitting", clientOrderId}`；返回后填 `accountResultRef` 转 `settled`。`clientOrderId` 传给 Account 做**幂等去重**（需 Account 协同）。启动恢复扫描 `submitting`：用 `clientOrderId` 反查 Account 是否已有对应 order，有则补 settled，确无则 `settled, accepted=false, message="submission_no_account_effect"`。**无"猜失败"盲区**。
- **不耦合 Account**：Account 只认 `clientOrderId`，回 `orderId/fillIds/eventIds`；不知 `tradeId/runId/strategyVersion`。
- 两态即可（`submitting→settled`）；后续订单终态由 `account_trigger` run 实时处理，不回写 AgentTrade。
- `accepted=true` 且有 `orderId` 时写 `orderId→runId` 反查索引（`orderId,runId,tradeId,clientOrderId,createdAt`）。

### 复盘报告（review sub-agent 产物，= workspace 文件，不入库）

- review sub-agent 用 `write_file` 写报告到 `<workspace>/reviews/<tradeDate>.md`（收盘 run 默认写；对话临时 review 可只返回结论不写文件）。
- 报告**强制**含：① 当日交易清单(带 PnL)+ 按 `strategyVersion` 归因；② **组合收益 vs 基准**(组合当日收益率 vs 沪深300/中证500 等 `Quotes.core_indexes()`，超额=组合−基准；**②、③、④ 的数字段全部由 Runtime 确定性计算并写盘，不靠 agent 自报**——避免「按当天结果编数字 / 事后补救污染归因」)；③ **样本量与置信度声明**(当日有效交易笔数 < `review_min_sample_trades`(缺省30) → 顶部打印「样本不足，不构成策略有效性证据，仅供过程复盘」，**禁止输出"策略有效/无效"绩效结论**)；④ **上次建议 follow-up**(由 Runtime 确定性对账：列上版 `ReviewSuggestion` 文本 + 是否已 upsert 采纳 + 采纳后活跃版本；「采纳后表现」的定性由 agent 结论段补充)；⑤ **决策质量维度**(纪律遵守/止损执行/no_action 是否恰当)；⑥ 策略评估 + 建议(**不自动改策略**；有策略调整建议时经 `record_review_suggestion` 登记，供下次 follow-up 对账)。
- 文件**原子写**(临时文件+rename)；run failed 不留半成品；同交易日补做覆盖。前端左侧列表读 `<workspace>/reviews/`。
- **报告结构** = `Runtime 确定性段`（header + ②③④ 数字）+ `Agent 复盘结论段`（①⑤⑥ 定性 + ②③④ 引用 Runtime 数字）。Runtime 段保证每次 review 都有可审计的客观 artifact，不依赖 agent 调 `write_file`。

#### 组合当日收益率（②，确定性计算）

- **组合当日收益率 =(现权益 − 日初权益)/日初权益**，`超额 = 组合当日收益率 − 各基准当日涨幅`，逐指数列出。
- **组合当日收益率由 Account 只读 facade `daily_return(now)` 直接返回**——Account 是账户财务事实单一所有者（见 account-module.md「账户财务事实只读 facade」），它内部持久化当日日初权益基线（幂等、重启安全）、用 `AccountSnapshot.totalAssets` 当现权益算收益率。**Runtime 不再自己落日初权益基线、不再自己相减算组合收益率**。
- **超额 = 组合收益率 − 基准涨幅** 是**正当的跨 BC 编排，留在 Runtime**：组合收益率来自 Account `daily_return()`、基准涨幅来自 `Quotes.core_indexes()` 各指数当日 `changePercent`，两个 BC 事实的相减由 Runtime 写进复盘报告。
- `daily_return()` 返回 `None`（日初基线 / 权益缺失）或基准缺失 → 对应行降级标注「不可算」，**不编造数字**。

### `ReviewSuggestion`（复盘策略建议 → 下次 follow-up 对账）

```ts
type ReviewSuggestion = {
  suggestionId: string;
  reviewRunId: string;       // 产出该建议的 review run
  tradeDate: TradeDate;      // 该建议所属交易日（CN）
  text: string;              // 策略建议文本（清晰、可执行）
  createdAt: OccurredAt;
};
```

规则：

- **产生机制**：review run 形成对策略的调整建议时，经 **`record_review_suggestion` 工具**（仅 review mode 暴露）声明 `text` → Runtime 持久化为 `ReviewSuggestion`（绑定本 review run + 该 review 的交易日）。无建议则不调用。
- **为何用工具而非从结论文本抽取**：结构化工具声明清晰、可靠、可审计；从自由结论文本正则抽取脆弱（易漏/误判）。建议文本与「采纳与否」解耦——`record_review_suggestion` **只登记建议，不改策略**（策略只在对话 mode 用户确认后经 `upsert_investment_strategy` 写）。
- **follow-up 确定性对账**（spec §3 复盘报告 ④）：下次 review 时 Runtime 读**上一交易日**的 `ReviewSuggestion` + 查**策略版本历史**（`StrategyService.list_versions`）→ 判断该建议产出后（按时间 `version.updatedAt > suggestion.createdAt`）是否发生过 `upsert_investment_strategy` 采纳 → 在 follow-up 段确定性写出：上版建议文本 + 是否已采纳（有无后续 upsert）+ 采纳后活跃策略版本。「采纳后表现」结合组合收益由 agent 结论段做定性。
- 持久化、append-only；前端可后续接入展示（本阶段不强制 UI）。

### 不变量

- 每次 run 必有 `runId`、mode、trigger、status、起止、冻结 `strategyVersion`（若有策略）。
- 纯聊天/知识问答可只有 run，不产生 AnalysisResult/AgentTrade。
- 每次 `operate_account` 必产生 `AgentTrade`（`submitting`→`settled`）；交易写必经 Account。
- **review run 永不 `operate_account`**。
- 工具结果必带时间戳；过期事实不得作为下单依据。
- 策略只在对话 mode 经用户确认更新；其余 mode 不写策略。
- news mode 形成判断必 emit `AnalysisResult`（含 `no_action`）；同一 news 不重复分析。
- `account-triggered` 必须被实时幂等消费；`mark_trigger_handled` 只在 run 终态后调用。

---

## 4. 工具

| 工具 | 来源 | 能力 | 副作用 |
|---|---|---|---|
| `fetch_quotes` | Quotes | 行情/K线/指标/基本面/扫描 | none |
| `fetch_news` | News | 新闻列表/全文/**关键词搜索(FTS)** | none |
| `fetch_account` | Account | 总览/持仓/订单/自选/事件/触发 | none |
| `operate_account` | Account | 挂单/撤单/开仓/调仓/平仓/调整保护 | trading_write |
| `update_watchlist` | Account | 增删自选/备注 | non_trading_write |
| `record_analysis` | Runtime | news 分析定论：声明 action/no_action + 理由 + 相关标的（**仅 news mode**） | non_trading_write |
| `record_review_suggestion` | Runtime | 复盘策略建议登记：声明 text（**仅 review mode**；只登记供下次 follow-up 对账，不改策略） | non_trading_write |
| `upsert_investment_strategy` | Runtime | 写策略新版本（**仅对话 mode、用户确认后**） | non_trading_write |
| `read_file`/`write_file`/`edit_file`/`run_bash` | 本地（沙箱，机制见 Infra） | 工作区文件/命令 | none / non_trading_write |
| `run_subagent`/`create_skill`/`run_skill` | Infra 默认 | fork 子 agent / 沉淀·执行 skill | 取决于子 agent |

> 本地工具沙箱、危险命令门禁、fork/skill 渐进披露契约见 [agent-infra-module.md](agent-infra-module.md)。

### 领域工具 schema（节选关键变更）

```ts
type OperateAccountToolInput = {
  accountInput: OperateAccountInput;      // clientOrderId 由 Runtime handler 生成并注入 accountInput.clientOrderId，模型不提供
  reason: string;
};
type OperateAccountToolOutput = {
  accepted: boolean; reason?: ErrorCode; message?: string;
  orderId?: string; fillIds?: string[]; positionId?: string;
  rejectionEventId?: string; accountEventIds: string[];
  snapshot: AccountSnapshot;              // Account canonical DTO
  warnings?: WarningCode[];
  tradeId: string;
};

type UpsertInvestmentStrategyToolInput = {
  strategyId?: string; baseVersion?: number;
  strategy: string;                       // 自然语言全文
  status?: "active" | "paused"; reason: string;
};
type UpsertInvestmentStrategyToolOutput = { accepted: boolean; strategyId: string; version: number; reason?: ErrorCode };

type RecordAnalysisToolInput = {
  kind: "action" | "no_action";
  summary: string;                        // 结论 + 理由（含「为什么现在进还来得及/已 price-in」判断）
  relatedCodes: TsCode[];                 // 可空
};
type RecordAnalysisToolOutput = { resultId: string };

type RecordReviewSuggestionToolInput = {
  text: string;                           // 策略建议文本（清晰、可执行）；仅 review mode
};
type RecordReviewSuggestionToolOutput = { suggestionId: string };  // Runtime 绑定 reviewRunId + tradeDate 落库

// 临时复盘不另设工具：对话/news 直接用 Infra `run_subagent`，allowedTools 收紧为只读
// `["fetch_quotes","fetch_news","fetch_account"]`，prompt 写明复盘 scope，拿回结论文本。
```

`fetch_quotes` / `fetch_news` / `fetch_account` / `update_watchlist` 的 input/output 与上一版一致（引用各 BC canonical DTO），此处不复述。`fetch_quotes` 工具 → Quotes `fetch_data`（按 tsCodes）+ `scan_market`（按 scan），二者由 input 二选一路由。

规则：

- `operate_account` handler 流程：① **生成 `clientOrderId` 并写入 `accountInput.clientOrderId`**，落 `AgentTrade{submitting, clientOrderId}` → ② 调 Account（`accountInput` 已含 clientOrderId，Account 按它幂等去重 + fail-closed 账户级硬约束——唯一机器闸门）→ ③ 填 result 转 `settled`，写 `orderId→runId` 索引 → ④ 回 `tradeId`。按账户**全局串行**。`clientOrderId` 由 Runtime 单点生成，模型不提供。Runtime 不做下单前风控拦截。
- 临时复盘子 agent（对话/news 经 `run_subagent` fork）：上下文隔离，`allowedTools` 收紧为只读 `fetch_*`（**无 operate_account / write_file**，只读由 allowedTools 保证）；子 agent 内剔除全部 spawn 类工具 `{run_subagent, run_skill}`（Infra 按 spawn-class 标记剔除，见 agent-infra §3.5，无嵌套）；只把最终结论文本回给父 run。收盘 review 走顶层 review-mode run（见 §6），由 Runtime 落盘报告。
- `record_analysis` 只在 **news mode** 暴露，per-run 绑定 `run_id`：handler 解析 `{kind, summary, relatedCodes}` → 经 `record_analysis_result` 持久化 + emit `agent-analysis-result`；`tradeIds` 由 Runtime 按 `run_id` 关联本 run 已记的 `AgentTrades`（handler 查 `list_trades_by_run`），模型不提供。`kind` 非法 → `InvalidInput` 业务拒绝。
- `upsert_investment_strategy` 只在对话 mode 暴露，要求"用户已确认"。
- 错误码取 [shared-types.md](shared-types.md) §5 封闭集合。

### 证据 = ToolCall 审计（不单独持久化）

- 每次工具调用由 Infra 记 `ToolCall` + 完整 payload 进 `PayloadStore`。决策链证据 = 按 `run_id` 拉该 run 的 `fetch_*` 调用。**被复盘报告引用的 run 的 payload pin 不被 GC**（§8）。

---

## 5. news 分析机制（buffer：滚动 4h · drain · 最新优先）

```text
开启自动分析 → 回填最近 4h news 入 buffer
News 刷新到的新 news（开启时）→ 直接 push 进 buffer
触发：未分析数 ≥ M 立即 OR 每 N 兜底
消费：取「最新 ≤M 条」(newest-first) → news run 分析 → emit AnalysisResult → drain
age-out：未分析且超 4h → 丢弃（并 emit 计数，不静默）
```

```ts
type NewsBufferItem = {
  newsId: string; enteredAt: OccurredAt; publishedAt?: OccurredAt;
  status: "pending" | "in_batch" | "analyzed" | "dropped"; runId?: string;
};
```

规则：

- **默认关闭**；buffer durable。生产者：`news-refreshed` 后 push `newIds∪updatedIds∪articleUpdatedNewsIds`（去重），纯 failed/warnings 变化不入队。
- 触发：`pending ≥ news_agent_batch_size(M=50)` 立即；否则每 `news_agent_max_wait_secs(N=600)` 兜底。
- 消费：取 publishedAt 最新 ≤M 条 → `AgentRun(mode=news)`，标 `in_batch`+`runId`；成功 `analyzed`，可恢复失败回 `pending`，不可恢复 `dropped`。**newest-first 排序锚点 = `publishedAt` 降序；`publishedAt` 缺失的条目排到末尾，以 `enteredAt` 降序兜底。**
- **不可恢复终态**（本批直接 `dropped`，不回 pending）：run 被用户取消（重试违背取消意图）、run 超 token 预算（同批重跑必然再超限 → 烧钱循环）。其余 failed（provider 抖动 / max_turns）回 pending 重试。
- **age-out**：超 4h 仍 pending → `dropped`，**emit 计数**（`agent-news-buffer-dropped` / heartbeat）。**4h 窗口锚点 = `enteredAt`（入队时间，滚动 4h）**。高负载下"最新优先"会饿死中段新闻，这是**有意接受的有损降级**，但必须让用户看见丢了多少。
- news run 可 `fetch_news({query})` 关键词深挖。in-flight：`agent.news_batch` lock；启动恢复 `in_batch` 失效回 `pending`。

---

## 6. 编排流

### 关联：共享结构化日志，不共享对话

- 自主组（news + account_trigger + review）共享"结构化交易日志"（AnalysisResults + AgentTrades + 账户结果 + `orderId→runId` 索引）；不共享对话流；深挖按 `run_id` 拉 transcript。对话 mode 独立连续线程，也能读策略/账户/最近结果。

### 对话（dialogue）

```text
send_agent_message → AgentRun(mode=dialogue) → 注入 L1+L2+会话历史+当日意图
  → run_agent_turn → 流式返回
  → 交易：operate_account → AgentTrade
  → 改策略：与用户确认 → upsert_investment_strategy
  → 用户说"review 一下" → run_subagent(allowedTools=只读 fetch_*) → fork 只读复盘子 agent → 返回结论
```

### news

```text
news-refreshed → buffer(§5) → M/N 触发 → AgentRun(mode=news)
  → 注入本批 news+账户/行情+当日意图
  → 形成判断 → 调 record_analysis(kind/summary/relatedCodes) → emit AnalysisResult（action/no_action）
  → action 下单（账户 fail-closed 兜底）→ operate_account → AgentTrade
  → 需要时 run_subagent(只读 fetch_*) 临时复盘历史决策辅助判断
```

### account_trigger（实时）

```text
Account emits account-triggered
  → 读 triggerId → dedupe(triggerId)
  → orderId→runId 反查原始建仓 run（写 causationRunId）
  → AgentRun(mode=account_trigger) → 注入本 trigger+原始 run 摘要+账户/行情+当日意图
  → 决定平仓/调仓/调整保护（止损命中=最高优先实时处置）
  → run 终态后才 Account.mark_trigger_handled(triggerId)
```

- Runtime 在**账户自驱 quote tick**（见「行情 / 账户维护调度」：对 `subscribed_codes ∪ core_indexes` 做 focused refresh → `rebuild_account_snapshot` 后）+ `account_trigger_eval_interval_secs` 兜底 调 `evaluate_account_triggers`，按 `hasMore`/`nextCursor` 分页耗尽（入参游标 `cursor`，对齐 account-module `AccountTriggerResult`）。**不再消费 universe 的 `market-quotes-refreshed` 事件来重建账户 / 评估触发器**——账户评估的数据依赖只有 `持仓 ∪ 挂单 ∪ 自选`（有界），与全市场刷新无关。
- 同一 `triggerId` 只路由一次；`mark_trigger_handled` 只在 run 终态后；仅启动 run 不得标 handled。
- **止损/止盈命中、挂单成交/拒单实时被感知处置**——止损在日内有效，模拟账户风险画像真实。

### review（只读；收盘调度 / 手动顶层 run）

```text
收盘 tick / 手动 run_review 命令 → AgentRun(mode=review, trigger=eod_review{tradeDate})  [顶层]
  → 注入 scope 决策链 + 账户结果 + 基准(core_indexes) + 策略 + 样本量声明
  → 评估「策略合不合理」（基准超额 + 样本量声明 + 上次建议 follow-up + 决策质量）
  → Runtime 捕获 agent 结论文本 → 确定性落盘 markdown 报告
  → ★永不 operate_account★
```

- `eod_review_time`（CN ≥15:30）触发收盘 review；按交易日加锁（一日一次）；可经 `run_review` 命令手动起。
- 对话/news 中的**临时复盘**不走顶层 run，而是 `run_subagent`（allowedTools 收紧为只读 `fetch_*`）拿回结论文本——只读由 allowedTools 保证，不另设 `run_review` 工具。

### 风控（无编排级闸门；机器闸门只有 Account fail-closed）

> **设计决策（2026-06-10）**：不做编排级风控闸门（熔断 / 追高 / 当日额度强拦截）——模拟账户，亏损是学习闭环的反馈信号，策略好坏由复盘闭环评判。账户级硬约束（`AccountRiskPolicy`，含日新单上限等）由 Account fail-closed 兜底，是唯一机器闸门。

- **防自我打架（信息注入，非拦截）**：会下单的 run 的 L3 **强制注入**「当日已下 AgentTrades + 活跃挂单 + 当前持仓 + 剩余可下单额度」，避免 fresh run 重复建仓 / 自我对打。剩余额度是**提示信息**（来自账户 `maxDailyNewOrders` 与当日已下单数），真正的拒单由 Account 执行。
- 追高 / price-in 判断是 L1 纪律（自然语言），由模型自律；不做机器拦截。

### 行情 / 账户维护调度

```text
quote tick   → codes = Account.subscribed_codes() ∪ Quotes.core_indexes()
               codes 为空（空仓且无挂单且无自选）→ 跳过本 tick（零成本）
               否则 → Quotes.refresh_quotes(codes)  ← focused、同步、恒 final=true、亚秒级
                    → Account.rebuild_account_snapshot()
                    → Account.evaluate_account_triggers() 分页耗尽（→ 可能 account-triggered）
收盘后        → Quotes.refresh_market_quotes(purpose=close)（按交易日加锁，启动补做）
维护 tick     → Quotes.refresh_market_instruments / refresh_klines / refresh_daily_basic / refresh_company_events；News.warm_articles
```

- **账户自驱、与 universe 解耦**：账户评估由上面的 quote tick 自驱——Runtime 对账户有界关注集合（`subscribed_codes ∪ core_indexes`）做 **focused refresh**（同步 final=true，亚秒级），刷完立即 rebuild→eval。**不搭 universe 全市场刷新的便车**：universe 的 `market-quotes-refreshed`（含后台 fallback 末段）退化为**纯 UI / 行情读模型事件**，Runtime 不再以它驱动账户重建（否则账户要等一批与自己无关的 BJ fallback 跑完，延时几十秒）。
- quote tick cadence 取 `account_trigger_eval_interval_secs`（兼作账户自驱与兜底评估节拍）。
- scope 由 Runtime 派生；维护任务 P4，失败不阻塞主流程但写 heartbeat。

---

## 7. 应用事件模型

| Event | Payload | Producer | Consumer | 含义 |
|---|---|---|---|---|
| `news-refreshed` | `NewsRefreshedPayload` | News | Runtime/UI | 新闻读模型变化 |
| `market-quotes-refreshed` | `MarketQuotesRefreshedPayload` | Quotes | UI / 行情读模型 | 行情更新（账户不再消费它驱动重建，见 §6/§8） |
| `account-updated` | `AccountUpdatedPayload` | Account | Runtime/UI | 账户状态变化 |
| `account-triggered` | `AccountTriggeredPayload` | Account | Runtime/UI | 订单/仓位条件命中，需实时决策 |
| `agent-run-started` | `{runId, mode, trigger}` | Runtime | UI/可观测 | run 起 |
| `agent-run-finished` | `{runId, status, error?}` | Runtime | UI/可观测 | run 止 |
| `market-quotes-refresh-progress` | `MarketQuotesRefreshProgressPayload` | Quotes | UI + Runtime(headless) | universe scope 两段刷新进度 |
| `agent-analysis-result` | `{resultId,runId,kind}` | Runtime | UI | news 分析结果产出 |
| `agent-news-buffer-dropped` | `{count, windowSecs, occurredAt}` | Runtime | UI | news age-out 丢弃计数（§5） |

规则：事件只表达事实；producer 不知 consumer；consumer 幂等；envelope 含 `eventId`+`occurredAt`，可带 `correlationId`/`causationId`；跨 BC 路由事件须 durable consumption record。

---

## 8. 幂等与可靠性

### In-flight lock

| Task | Lock Key |
|---|---|
| news batch run | `agent.news_batch` |
| account trigger evaluation | `account.trigger_eval` |
| account trigger run | `agent.account_trigger:{trigger_id}` |
| 收盘复盘 run | `agent.eod_review:{trade_date}` |
| quote/close/维护 | `quotes.*` / `news.article_warm` |

### 事件消费记录

```ts
type EventConsumption = {
  eventType: string; eventKey: string; consumer: string;
  status: "processing" | "consumed" | "ignored" | "failed";
  runId?: string; error?: string; createdAt: OccurredAt; updatedAt: OccurredAt;
};
```

- 幂等键：`account-triggered`→`triggerId`；`news-refreshed`→`batchId`（或排序 changed-ids hash）；`account-updated`→`accountEventIds` hash。
- **`market-quotes-refreshed` 不再被 Runtime 当作账户重建事件消费**（账户走自有 focused refresh quote tick 自驱，见 §6）——它仅供前端 / 行情读模型更新，**无需 durable consumption record / 幂等键**。

> ⚠️ 历史说明：上一版曾以 `market-quotes-refreshed`（`scope+purpose+tradeDate` 键、只在 `final=true` 触发 rebuild）驱动账户重建；账户自驱解耦后该 EventConsumption 用法退役。`final` 字段在 payload 中**保留**（focused refresh 恒 true，universe 仍两段 emit，UI 仍可用），只是账户侧不再以它为闸门。
- `consumed` 不重复触发；`ignored` 终态；`processing` 超时可回收；`mark_trigger_handled` 在 consumption 进终态后。

### 失败 / 恢复

- 单次失败不崩 runtime；连续失败退避；可恢复留 pending；不可恢复写失败状态 + UI 事件。
- 启动恢复顺序：① 用 `clientOrderId` 对账 `submitting` 悬挂 `AgentTrade`；② 旧 `running` run 标 `failed(interrupted_by_restart)`；③ 恢复未终态事件消费；④ 恢复 news buffer `in_batch`/`pending`；⑤ 从 Account 补扫未 handled trigger；⑥ 补做缺失收盘复盘。
- 错过盘后任务下次启动/tick 补偿。

### 证据 payload 保留

- 第一阶段 PayloadStore 不做 GC。**若未来引入 GC，被复盘报告引用的 run 的 ToolCall payload 必须 pin**，或复盘时把关键证据原文写进报告文件。

### Runtime settings keys

| Key | 含义 | 缺省 |
|---|---|---|
| `news_auto_analysis_enabled` | news 自动分析开关 | false |
| `news_agent_batch_size`(M) / `news_agent_max_wait_secs`(N) / `news_buffer_window_secs` | news buffer 阈值 | 50 / 600 / 14400 |
| `account_trigger_eval_interval_secs` / `account_trigger_eval_batch_size` | trigger 评估兜底 | 10 / 200 |
| `eod_review_time` | 收盘复盘触发时间 | `15:30 Asia/Shanghai` |
| `review_min_sample_trades` | 复盘可下绩效结论的最小交易笔数 | 30 |
| `agent_run_max_turns` / `agent_run_token_budget` / `agent_daily_token_budget` | run/日 token 护栏（run 预算口径 = 累计 input+output + 子 run 回灌） | 40 / 1000000 / 5000000 |
| `quotes_*_refresh_time` / `news_article_warm_*` | 维护 cadence | 见旧值 |
| `context_soft/summarize/hard_limit_tokens` / `agent_context_compact_channel_id` / `_model` | Infra 压缩 | 48000/64000/96000 / unset |

- settings 是运行时配置，不属于 `InvestmentStrategy`，模型不能隐式改；缺失用缺省，非法 fail-closed+heartbeat。
- **token 预算执行机制在 Infra**：超预算停 turn（`run_agent_turn` 的 `tokenBudget` 入参 + `token_budget_exceeded` stop_reason）、fork 子 run（含 review）usage 回灌父 run 累加，均由 Infra 执行（见 agent-infra）；Runtime 只配置 `agent_run_token_budget` / `agent_daily_token_budget` 等 settings 值并传入 Infra。
- ⚠️ **`agent_daily_token_budget`（日预算）当前未接执行**（需跨 run 的当日用量累计持久化）；run 级预算已生效。后续接：Runtime 在发起 run 前查当日累计，超限拒起自动 run（dialogue 仍放行 + 提示）。

---

## 9. 对外接口

```ts
type SendAgentMessageRequest = { content: string; images?: string[]; conversationId?: string };  // conversationId 续接已有 dialogue 线程（§3）；不传则新开匿名线程
type SendAgentMessageResponse = { messageId: string; runId: string };

type CancelAgentRunRequest = { runId: string; reason?: string };
type CancelAgentRunResponse = { accepted: boolean; runId: string; status: "cancelled" | "completed" | "failed" | "not_found" };

type FetchAgentStateRequest = {
  include?: { messages?: boolean; runs?: boolean; analysisResults?: boolean; trades?: boolean; strategy?: boolean; toolCalls?: boolean };
  limit?: number; offset?: number;
};

type FetchInvestmentStrategyRequest = {
  status?: "active" | "paused";
  includeHistory?: boolean;                  // true=附带该 strategyId 的历史版本列表
};
type FetchInvestmentStrategyResponse = {
  active?: InvestmentStrategy;
  history?: { version: number; updatedAt: OccurredAt; reason: string }[];  // includeHistory=true 时返回
};
type UpsertInvestmentStrategyRequest = {
  strategyId?: string; baseVersion?: number; strategy: string; status: "active" | "paused"; reason: string;
};

type ListReviewReportsRequest = { limit?: number };          // 读 <workspace>/reviews/
```

规则：

- `cancel_agent_run` 可取消 `queued` 或尚未提交当前 tool call 的 `running` run（含后台 news/account_trigger/review）；已提交 Account 的 `operate_account` 不可撤（撤单走新 `operate_account(cancel_order)`）。
- `upsert_investment_strategy`（command）= 用户显式确认改策略的唯一入口；`baseVersion` 乐观并发，冲突 `version_conflict`。
- 前端：中间 chat + 右侧 AnalysisResult 列表 + 左侧复盘报告文件列表 + 投资策略面板（自然语言+版本）。

### 内部 Runtime API

```rust
send_agent_message(req) -> SendAgentMessageResponse;
cancel_agent_run(run_id, reason) -> CancelAgentRunResponse;
run_agent_from_news(news_ids) -> AgentRunResult;
run_agent_from_account_trigger(trigger_id) -> AgentRunResult;
run_eod_review(trade_date) -> ReviewRunResult;         // 顶层 review run；Runtime 落盘报告，回 reportPath
// 临时复盘无独立 API：对话/news 经 Infra run_subagent(allowedTools=只读) fork，结论文本回父 run
build_run_context(mode, trigger) -> RunContext;        // L1+L2+L3（含当日意图），只读 facade
register_tools_for_mode(mode) -> ToolSpec[];
record_analysis_result(run_id, result) -> ResultId;
record_agent_trade(run_id, client_order_id, account_input, reason) -> TradeId;  // submitting
settle_agent_trade(trade_id, account_result) -> ();
upsert_investment_strategy(req) -> (StrategyId, Version);
recover_on_startup(now) -> RecoverySummary;
```

---

## 10. 实现映射

```text
pipeline/agent_runtime/
  runs.rs          AgentRun / mode / 生命周期 / 策略版本冻结
  context.rs       L1+L2+L3 组装（含当日意图注入）
  tools.rs         mode -> ToolRegistry + 领域 tool handler（含 record_analysis 仅 news mode；无 run_review；临时复盘用 Infra run_subagent）
  records.rs       AnalysisResult（record_analysis_result 持久化+emit）/ AgentTrade(submitting→settled) / orderId→runId 索引 / ReviewSuggestion 登记（日初权益已下沉 Account facade，不在此）
  strategy.rs      InvestmentStrategy 读写 / 版本（自然语言）
  news_buffer.rs   §5 buffer 生产/消费/age-out(+计数)
  triggers.rs      account-triggered 消费 / dedupe / mark_handled / 归因
  review.rs        review 子 agent（只读）+ 组合 vs 基准超额(确定性) + 上次建议 follow-up 对账(确定性) + 样本护栏 + 报告原子写
  events.rs / router.rs / locks.rs / heartbeat.rs
```

- 关键模块顶部标 `// Spec: agent-runtime-module.md §X`。

---

## 11. 验收标准

- 三类主动/响应 trigger（对话 / news / account_trigger）+ review（收盘调度 / 按需 fork）；`account-triggered` 被 Runtime **实时**幂等消费，run 终态后才 `mark_trigger_handled`。
- 止损/止盈命中、挂单成交/拒单**实时唤起 account_trigger run**；经 `orderId→runId` 归因到原始建仓 run。
- **review run 永不 `operate_account`**；顶层 review run 由收盘调度 / 手动 `run_review` 命令起，Runtime 捕获结论确定性落盘报告；对话/news 的临时复盘经 `run_subagent`（allowedTools 收紧只读 `fetch_*`）拿回结论文本（不另设 `run_review` fork 工具，只读由 allowedTools 保证）。
- run 创建时冻结 `strategyVersion`，全程不变。
- 每次 `operate_account` 必写 `AgentTrade`（`submitting`→`settled`）带 `clientOrderId`；崩溃后用 `clientOrderId` 确定性对账，无"猜失败"盲区。
- 自动下单无编排级风控闸门（设计决策，§6）；账户级硬约束由 Account fail-closed 兜底，是唯一机器闸门。
- 会下单的 run 的 L3 强制注入「当日已下 AgentTrades + 活跃挂单 + 持仓 + 剩余额度」（防自我打架）。
- 策略**纯自然语言**，只在对话 mode 经用户确认更新；其余 mode 不写策略。
- news mode 形成判断必 emit `AnalysisResult`（含 `no_action`）；age-out 丢弃 emit 计数，不静默。
- 复盘报告强制含：**组合当日收益率 + vs 基准超额（Runtime 确定性算，不靠 agent 自报）**、样本量与置信度声明（< `review_min_sample_trades` 禁绝绩效结论）、**上次建议 follow-up（Runtime 确定性对账 `ReviewSuggestion` ↔ 后续 upsert ↔ 活跃版本）**、决策质量维度；文件原子写；不自动改策略。
- 决策链可按 `run_id` 完整回溯；证据靠 run_id→ToolCall，复盘引用 payload pin 不被 GC。
- token/成本有预算护栏；后台任务失败有 heartbeat；启动恢复覆盖悬挂 AgentTrade / 中断 run / news buffer / 未 handled trigger / 缺失复盘。
- Quotes / News / Account 任一层不 import Agent 代码；Runtime 不拥有交易判断逻辑，不绕过 Agent 与 Account。
