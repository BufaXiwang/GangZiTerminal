# Runtime Orchestration Spec

> 本文档定义跨模块事件路由和后台任务编排。它不是新的 bounded context，不拥有业务规则。
>
> 模块级领域契约仍以 `docs/design/*-module.md` 为准。

## 一句话定位

**Runtime Orchestrator 是应用层事件编排器**：它监听模块事件、运行定时任务、合成订阅集合、触发 Agent run，并保证跨模块动作幂等、有节奏、可观测。

它解决的问题：

```text
News / Account / Quotes 只表达“发生了什么”
Runtime Orchestrator 决定“谁应该被唤起、何时唤起、如何去重”
Agent 决定“如何判断和行动”
```

---

## 1. 责任边界

Runtime Orchestrator 负责：

- 启动和管理后台 loop。
- 监听应用内事件，如 `news-refreshed`、`account-triggered`、`market-quotes-refreshed`。
- 把模块事件路由成 Agent run 或其他应用层 use case。
- 定时触发 News refresh、Quotes refresh、Account trigger evaluation、K 线预热、Agent scheduled review。
- 从 Account 获取 subscribed codes，并注入 Quotes refresh scope。
- 维护跨模块任务的 in-flight lock、退避、重试、幂等和 heartbeat。
- 向前端 emit 可观测运行状态。

Runtime Orchestrator 不负责：

- 判断新闻重要性。
- 判断是否买卖。
- 计算行情、指标、账户现金、PnL。
- 直接写 News / Quotes / Account / Agent 的内部表。
- 绕过 Agent 启动交易决策。

边界规则：

- Orchestrator 位于 `pipeline/` 或由 adapter 启动的 runtime wiring 中。
- Orchestrator 可以调用各模块公开 use case / facade。
- Orchestrator 不属于 Quotes / News / Account / Agent 任一 BC。
- Orchestrator 不把业务规则放进自己内部；复杂判断下沉到对应 BC 或 Agent。

---

## 2. 事件模型

### 应用事件

| Event | Producer | Consumer | 含义 |
|---|---|---|---|
| `news-refreshed` | News refresh use case | Orchestrator / UI | 新闻本地读模型发生变化 |
| `market-quotes-refreshed` | Quotes refresh use case | Orchestrator / Account snapshot / UI | 行情 snapshot 更新 |
| `account-updated` | Account write / trigger use case | Orchestrator / UI | 账户状态发生变化 |
| `account-triggered` | Account trigger evaluation | Orchestrator / UI | 订单或仓位条件命中，需要 Agent 感知 |
| `agent-run-started` | Agent run use case | UI / observability | Agent run 开始 |
| `agent-run-finished` | Agent run use case | UI / observability | Agent run 结束 |

规则：

- Event 只表达事实，不携带业务决策。
- Producer 不知道 consumer。
- Consumer 必须做幂等处理。
- Event payload 必须包含关联 ID 和发生时间。

### Payload 最小要求

```ts
type AppEventEnvelope<T> = {
  eventId: string;
  type: string;
  occurredAt: string;
  payload: T;
};
```

示例：

```ts
type NewsRefreshedPayload = {
  fetchedCount: number;
  savedCount: number;
  failedCount: number;
  firstFailure?: string;
  newIds?: string[];
};

type AccountTriggeredPayload = {
  triggerId: string;
  positionId?: string;
  orderId?: string;
  tsCode?: string;
  triggerType: "stop_loss" | "take_profit" | "time_stop" | "order_filled" | "order_rejected" | "order_expired" | "invalidated";
};

type MarketQuotesRefreshedPayload = {
  scope: "subscribed" | "universe" | "manual";
  total: number;
  success: number;
  failedBatches: number;
  capturedAt: string;
};
```

---

## 3. 编排流

### News -> Agent

```text
Runtime Orchestrator tick
  -> News.refresh_news()
  -> News emits news-refreshed
  -> Orchestrator checks pending / newIds / throttle
  -> Agent.run_agent_from_news(news_ids)
```

规则：

- News 只刷新和 emit，不启动 Agent。
- Orchestrator 负责攒批、阈值、节流和 in-flight lock。
- Agent 负责新闻分析、关联标的、交易影响判断。

### Account -> Agent

```text
Runtime Orchestrator tick
  -> Account.evaluate_account_triggers(now)
  -> Account appends AccountTrigger
  -> Account emits account-triggered
  -> Orchestrator dedupe(trigger_id)
  -> Agent.run_agent_from_account_trigger(trigger_id)
```

规则：

- Account 只判断条件是否命中，不决定响应动作。
- Orchestrator 负责同一 `trigger_id` 只路由一次。
- Agent 收到触发后读取 Account / Quotes / News，再决定是否操作账户。

### Account -> Quotes 订阅行情

```text
Runtime Orchestrator quote tick
  -> Account.subscribed_codes()
  -> add Quotes.core_indexes()
  -> Quotes.refresh_market_quotes(scope=subscribed)
  -> Quotes emits market-quotes-refreshed
```

规则：

- Account 拥有自选、持仓、挂单，因此暴露 subscribed codes。
- Quotes 负责按 scope 刷新行情 snapshot。
- Orchestrator 负责把 subscribed codes 注入 Quotes refresh。
- Account 不调用 Quotes provider；Quotes 不读取 Account 内部实现。

### Quotes -> Account snapshot

```text
Quotes emits market-quotes-refreshed
  -> Orchestrator schedules Account.rebuild_snapshot()
  -> Account emits account-updated
```

规则：

- Account snapshot 可因行情变化重新派生。
- 这不是交易决策，只是估值和前端展示更新。

### Scheduled Agent Review

```text
Runtime Orchestrator scheduled tick
  -> Agent.run_scheduled_agent_review(reason)
```

规则：

- 用于巡检持仓、挂单、自选和最近 episode。
- 定时 review 不是强制交易。
- 若同时存在高优先级 account trigger，优先处理 account trigger。

---

## 4. 调度优先级

| 优先级 | Trigger | 说明 |
|---:|---|---|
| P0 | `account-triggered` | 止损、止盈、拒单、成交等账户事件 |
| P1 | `user_chat` | 用户前台交互 |
| P2 | `news-refreshed` / news batch | 新闻驱动分析 |
| P3 | scheduled account / strategy review | 定时巡检和复盘 |
| P4 | quotes universe / kline warm / enrichment | 数据维护任务 |

规则：

- 同一账户同一标的的 P0 run 应串行。
- P0 可以打断或延后低优先级后台任务。
- P2 news batch 允许攒批，不要求每条新闻立即启动 Agent。
- 数据维护任务失败不应阻塞用户交互，但必须记录 heartbeat。

---

## 5. 幂等和可靠性

### In-flight lock

每类后台 run 至少有进程级锁：

| Task | Lock Key |
|---|---|
| news batch Agent run | `agent.news_batch` |
| account trigger Agent run | `agent.account_trigger:{trigger_id}` |
| scheduled review | `agent.scheduled_review` |
| quote subscribed refresh | `quotes.subscribed_refresh` |
| universe refresh | `quotes.universe_refresh` |

### 事件消费记录

跨模块事件路由需要记录消费状态：

```sql
create table orchestration_event_consumptions (
    event_type text not null,
    event_key text not null,
    consumer text not null,
    status text not null, -- processing / consumed / failed
    run_id text,
    error text,
    created_at text not null,
    updated_at text not null,
    primary key (event_type, event_key, consumer)
);
```

规则：

- `account-triggered` 使用 `trigger_id` 做 `event_key`。
- `news-refreshed` 可用 batch id 或 news id set hash 做 `event_key`。
- 已 consumed 的 event 不重复触发 Agent run。
- processing 超时可被 watchdog 回收。

### 失败策略

- 单次工具 / provider 失败不让整个 runtime 崩溃。
- 连续失败进入退避。
- 可恢复任务保留 pending 状态等待下次重试。
- 不可恢复错误写入失败状态和 UI 可见事件。

---

## 6. 可观测性

Orchestrator 每个 loop 必须有 heartbeat：

```ts
type SchedulerHeartbeat = {
  loopName: string;
  lastOkAt?: string;
  lastErrorAt?: string;
  lastError?: string;
  consecutiveFailures: number;
};
```

前端 Settings / Diagnostics 可展示：

- News refresh 状态。
- Quotes subscribed refresh 状态。
- Quotes universe refresh 状态。
- Account trigger evaluation 状态。
- Agent news batch 状态。
- Agent account trigger routing 状态。

---

## 7. 实现映射

推荐代码位置：

```text
pipeline/runtime/
  events.rs          AppEventEnvelope / constants / emit helpers
  orchestrator.rs    spawn_all / wiring
  router.rs          event listeners -> use case dispatch
  locks.rs           in-flight lock / watchdog
  heartbeat.rs       loop health

pipeline/scheduler.rs
  legacy entry; may delegate to pipeline/runtime
```

规则：

- 如果某个 loop 需要构造 adapter-only tool registry，可以由 adapter 启动，但它仍应遵循本 spec。
- 长期目标是把散落的 scheduler / listener 入口收敛到 runtime orchestration 命名空间。
- Runtime 只编排，不内嵌业务判断。

---

## 8. 验收标准

- News spec 中 `news-refreshed` 不直接触发 Agent；Runtime Orchestrator 监听并路由。
- Account spec 中 `account-triggered` 不直接触发 Agent；Runtime Orchestrator 幂等路由。
- Account 的 subscribed codes 由 Runtime Orchestrator 注入 Quotes refresh scope。
- Quotes refresh 完成后，Runtime Orchestrator 触发 Account snapshot 重建。
- 同一 `trigger_id` 不会导致重复 Agent 交易动作。
- 后台任务失败有 heartbeat 和日志，不会静默失效。
- Quotes / News / Account 任一层不 import Agent 代码。
- Runtime Orchestrator 不拥有交易判断逻辑，不绕过 Agent 和 Account。
