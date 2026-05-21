# Account 模块 Spec

> 本文档是 Account bounded context 的领域模型契约。模块边界 / 依赖方向以 `docs/design/architecture.md` 为准。
>
> 本文档只描述最新设计目标，作为实现依据。

## 一句话定位

**模拟券商账户 / Agent 交易执行模型**：Account 提供完整模拟账户能力，让 Agent 像人使用交易软件一样管理自选、挂单、开仓、调仓、平仓和保护条件；UI 和 Agent 读取本地账户状态。

Account 只负责交易执行、账户估值、事件审计和触发条件发现。投资判断、是否响应该触发、后续交易动作都属于 Agent。

---

## 1. 责任边界

Account 负责：

- 维护模拟账户现金、订单、成交、仓位、账户事件。
- 提供挂单、撤单、开仓、调仓、平仓、调整止损止盈 / 时间止损的能力。
- 管理自选列表，并提供带行情状态的自选列表读模型。
- 基于 Quotes snapshot 计算仓位价格、可卖数量、成本、盈亏、账户总资产。
- 提供订单成交条件和仓位保护条件的 evaluation use case。
- 条件触发后写账户事件并 emit 通知；事件路由由 Runtime Orchestrator 负责。
- 维护前端和 Agent 可读取的账户 snapshot、仓位列表、订单列表、自选列表。

Account 不负责：

- 判断是否应该买入、卖出、加仓、减仓。
- 响应止损 / 止盈触发后的具体行为。
- 调用 Agent、启动 Agent run 或管理跨模块调度。
- 获取行情源或维护 Quotes provider。
- 新闻获取、新闻分析、市场扫描。
- 真券商连接、真实下单、真实资产同步。

---

## 2. 领域模型

### 核心概念

| 概念 | 含义 | 主键 / 身份 |
|---|---|---|
| `SimAccount` | 模拟账户聚合根 | 单账户 |
| `Order` | 委托 / 挂单，表达一次买卖意图 | `order_id` |
| `TradeFill` | 成交回报；一个订单可有多笔成交 | `fill_id` |
| `Position` | 当前或历史仓位 | `position_id` |
| `PositionLot` | 按成交日拆分的持仓批次，用于 T+1 可卖数量 | `lot_id` |
| `PositionProtection` | 仓位保护条件：止损、止盈、时间止损、失效条件 | `position_id` |
| `WatchlistItem` | 自选标的 | `ts_code` |
| `AccountSnapshot` | 当前账户总览：现金、市值、资产、盈亏、风险暴露 | 查询派生 |
| `AccountEvent` | append-only 账户审计事件 | `event_id` |
| `AccountTrigger` | 订单或仓位条件触发后给 Agent 的通知事件 | `trigger_id` |

### 不变量

- Account 只有模拟账户语义，不连接真券商。
- Agent 独占交易写能力；前端不暴露开仓、平仓、调仓、挂单、撤单入口。
- Account 可以读取 Quotes snapshot 做估值、成交模拟和触发判断，但不调用 Quotes provider。
- Account 任一层不 import Agent 代码；触发 Agent 行为只能通过事件通知。
- 所有账户状态变化必须先写 `account_events`，再更新 / 派生订单、仓位、snapshot。
- 现金、PnL、总资产是派生值；可缓存 snapshot，但不得把缓存当真源。
- 订单和仓位分开建模：挂单是 `Order`，成交后才影响 `Position`。
- 止损 / 止盈不是默认真实挂单，而是 `PositionProtection` 条件；命中后只触发事件，由 Agent 决定后续动作。
- A 股交易规则由 Account 校验：整手、T+1、交易时段、可成交性、费用、印花税。
- 交易写路径必须检查 Quotes snapshot freshness；`quote.stale == true`、quote 缺失或关键价格缺失时，不允许即时成交。
- 批量读取返回 per-item warning/error；单个标的行情缺失不让整批失败。

### 订单模型

```ts
type Order = {
  orderId: string;
  tsCode: string;
  side: "buy" | "sell";
  orderType: "market" | "limit";
  limitPrice?: number;
  quantity: number;
  filledQuantity: number;
  status: "pending" | "partially_filled" | "filled" | "cancelled" | "rejected" | "expired";
  intent:
    | "open_position"
    | "scale_in"
    | "scale_out"
    | "close_position"
    | "manual_order";
  positionId?: string;
  reason?: string;
  source: "agent" | "system";
  createdAt: string;
  updatedAt: string;
  expiresAt?: string;
};
```

规则：

- `open_position` / `scale_position` / `close_position` 可以作为 Agent 的便捷动作，但 Account 内部仍应落为订单 + 成交 + 仓位事件。
- `market` 表示用当前 fresh Quotes snapshot 模拟即时成交；若 quote stale / missing / 盘口不可成交则拒单或保持 pending，按请求策略决定。
- `limit` 表示挂单；由定时任务根据 Quotes snapshot 判断是否成交、部分成交、过期。
- `limit` 订单可以在 quote stale 时创建为 pending，但不能在 stale quote 上成交。
- A 股股票买入 / 卖出数量必须是 100 股整数倍。

### 成交模型

```ts
type TradeFill = {
  fillId: string;
  orderId: string;
  positionId?: string;
  tsCode: string;
  side: "buy" | "sell";
  price: number;
  quantity: number;
  commission: number;
  stampTax: number;
  occurredAt: string;
};
```

规则：

- 买入成交减少现金，卖出成交增加现金。
- 佣金双向收取，印花税仅卖出收取。
- 买入成交生成新的 `PositionLot`，成交日当天不可卖。
- 卖出成交按可卖 lot 扣减，减少或关闭对应仓位。

### 仓位模型

```ts
type Position = {
  positionId: string;
  tsCode: string;
  code: string;
  name: string;
  status: "open" | "closed";
  quantity: number;
  sellableQuantity: number;
  avgCost: number;
  marketPrice?: number;
  marketValue?: number;
  realizedPnl: number;
  unrealizedPnl?: number;
  openedAt: string;
  closedAt?: string;
  protection?: PositionProtection;
  source: "agent" | "system";
  reasoning?: string;
};
```

规则：

- `avgCost` 由成交记录加权派生。
- `sellableQuantity` 由 `PositionLot` 和 T+1 规则派生。
- `marketPrice`、`marketValue`、`unrealizedPnl` 来自 Quotes snapshot 派生。
- 同一 `ts_code` 默认只有一个 open position；重复买入表现为加仓。
- 全部数量卖出后仓位关闭。

### 保护条件模型

```ts
type PositionProtection = {
  stopLoss?: number;
  takeProfit?: number;
  timeStopAt?: string;
  invalidationSignals?: string[];
  enabled: boolean;
  updatedAt: string;
};
```

规则：

- 对多头仓位：`stopLoss < currentPrice`，`takeProfit > currentPrice`。
- 条件命中只生成 `AccountTrigger` 和 `AccountEvent`，不默认自动平仓。
- Agent 可在收到触发后调用 `operate_account` 平仓、减仓、撤单或调整保护条件。
- 同一保护条件连续命中时必须去重，避免同一价格 tick 反复触发 Agent。

### 自选模型

```ts
type WatchlistItem = {
  tsCode: string;
  code: string;
  name?: string;
  addedBy: "agent" | "user" | "system";
  addedAt: string;
  note?: string;
};
```

Account 拥有自选列表。Quotes 只提供自选标的的实时行情、涨跌幅、成交量、成交额等市场状态。

### 账户快照

```ts
type AccountSnapshot = {
  initialCash: number;
  cash: number;
  availableCash: number;
  frozenCash: number;
  marketValue: number;
  totalAssets: number;
  realizedPnl: number;
  unrealizedPnl: number;
  totalPnl: number;
  openPositionCount: number;
  pendingOrderCount: number;
  capturedAt: string;
};
```

---

## 3. 数据流

### 写入流

```text
Agent operate_account
  -> adapter tool DTO
  -> Account use case
  -> domain rule validation
  -> append account_events
  -> upsert orders / fills / positions / lots / watchlist
  -> rebuild account snapshot
  -> emit account-updated
```

规则：

- 所有写操作串行化，避免现金、仓位、订单并发漂移。
- 写操作必须有 `source = agent | system` 和可审计 note/reason。
- 失败的写操作也应能返回明确 rejection reason；是否写 rejection event 由订单是否已创建决定。

### 读取流

```text
UI / Agent fetch_account
  -> account query facade
  -> account tables + ACCOUNT_SNAPSHOT
  -> optional Quotes snapshot join for market fields
  -> response with freshness/warnings
```

规则：

- UI / Agent 读取 Account 不触发远端行情 provider。
- 仓位和自选的行情字段只从 Quotes snapshot 左连接。
- Quotes snapshot 缺失时返回 `quote_missing` warning，账户基础数据仍返回。

### 触发评估流

```text
Runtime Orchestrator tick
  -> pending orders + open positions + watchlist
  -> Quotes snapshot
  -> simulate order fills / expirations
  -> evaluate PositionProtection
  -> append AccountEvent / AccountTrigger
  -> emit account-triggered / account-updated
```

规则：

- Account 只判断条件和发事件，不调用 Agent。
- `account-triggered` 是通知，不是交易指令。
- 触发事件必须带 `trigger_id`，Agent 可用它做幂等处理。
- Runtime Orchestrator 监听 `account-triggered` 并负责幂等触发 Agent run。

---

## 4. 对外接口

### 前端展示接口

前端只暴露一个 Account 读取 command：

```ts
type FetchAccountRequest = {
  include?: {
    snapshot?: boolean;
    positions?: boolean;
    orders?: boolean;
    watchlist?: boolean;
    events?: boolean;
    triggers?: boolean;
  };
  status?: "open" | "closed" | "pending" | "all";
  limit?: number;
  offset?: number;
};

type FetchAccountResponse = {
  snapshot?: AccountSnapshot;
  positions?: Position[];
  orders?: Order[];
  watchlist?: Array<WatchlistItem & {
    quote?: {
      price?: number;
      changePercent?: number;
      volume?: number;
      amount?: number;
      source?: string;
      freshness?: string;
    };
  }>;
  events?: AccountEvent[];
  triggers?: AccountTrigger[];
  warnings?: string[];
};
```

约束：

- 前端不能通过 Tauri command 做交易写操作。
- 前端展示自选时，Account 返回自选元信息，行情字段来自 Quotes snapshot。
- 是否允许用户维护自选属于产品交互选择；即使允许，也不能扩展到交易写能力。

### Agent 调用方法

Agent 使用两个 Account 工具：一个读，一个写。

#### `fetch_account`

```ts
type FetchAccountToolInput = FetchAccountRequest;

type FetchAccountToolOutput = {
  snapshot?: AccountSnapshot;
  positions?: Array<{
    positionId: string;
    tsCode: string;
    name: string;
    status: "open" | "closed";
    quantity: number;
    sellableQuantity: number;
    avgCost: number;
    marketPrice?: number;
    marketValue?: number;
    unrealizedPnl?: number;
    realizedPnl: number;
    protection?: PositionProtection;
    openedAt: string;
    closedAt?: string;
  }>;
  orders?: Array<{
    orderId: string;
    tsCode: string;
    side: "buy" | "sell";
    orderType: "market" | "limit";
    limitPrice?: number;
    quantity: number;
    filledQuantity: number;
    status: Order["status"];
    intent: Order["intent"];
    positionId?: string;
    createdAt: string;
    expiresAt?: string;
  }>;
  watchlist?: Array<WatchlistItem & {
    quote?: {
      price?: number;
      changePercent?: number;
      amount?: number;
      capturedAt?: string;
      stale?: boolean;
    };
  }>;
  triggers?: AccountTrigger[];
  recentEvents?: AccountEvent[];
  warnings?: string[];
};
```

约束：

- 输出使用 `FetchAccountToolOutput` 的 token-friendly 视图，不直接返回 UI DTO 或完整事件流。
- 可按 include 精确选择 snapshot / positions / orders / watchlist / events / triggers。
- 不触发远端行情 provider。

#### `operate_account`

```ts
type OperateAccountInput =
  | {
      action: "place_order";
      tsCode: string;
      side: "buy" | "sell";
      orderType: "market" | "limit";
      limitPrice?: number;
      quantity: number;
      expiresAt?: string;
      reason: string;
    }
  | {
      action: "cancel_order";
      orderId: string;
      reason: string;
    }
  | {
      action: "open_position";
      tsCode: string;
      quantity: number;
      orderType?: "market" | "limit";
      limitPrice?: number;
      stopLoss?: number;
      takeProfit?: number;
      timeStopAt?: string;
      reason: string;
    }
  | {
      action: "scale_position";
      positionId: string;
      quantityDelta: number;
      orderType?: "market" | "limit";
      limitPrice?: number;
      reason: string;
    }
  | {
      action: "close_position";
      positionId: string;
      quantity?: number;
      orderType?: "market" | "limit";
      limitPrice?: number;
      reason: string;
    }
  | {
      action: "adjust_protection";
      positionId: string;
      stopLoss?: number;
      takeProfit?: number;
      timeStopAt?: string;
      enabled?: boolean;
      reason: string;
    }
  | {
      action: "add_watchlist" | "remove_watchlist";
      tsCode: string;
      reason: string;
    };
```

约束：

- 所有 action 都必须写入可审计事件。
- `open_position`、`scale_position`、`close_position` 是便捷交易动作，内部仍走订单 / 成交流。
- `adjust_protection` 只调整保护条件，不直接下单。
- 工具返回必须包含 `accepted/rejected`、订单或仓位 ID、错误原因、最新 snapshot 摘要。
- 即时成交类 action 遇到 stale / missing quote 必须返回 `rejected`，reason 使用 `quote_stale` / `quote_missing` / `quote_price_missing`。

### 内部 Rust API

内部 API 以 command / query facade 为主：

```rust
fetch_account(request) -> FetchAccountResponse;
operate_account(request, source) -> OperateAccountResponse;
evaluate_account_triggers(now) -> AccountTriggerResult;
rebuild_account_snapshot() -> AccountSnapshot;
subscribed_codes() -> Vec<TsCode>;
```

---

## 5. 模块独有功能

### 交易规则

| 规则 | 说明 |
|---|---|
| 整手 | 股票买入 / 卖出数量必须是 100 股整数倍 |
| T+1 | 当日买入的 lot 当日不可卖 |
| 交易时段 | 即时成交类订单只在 A 股交易时段成交；挂单可盘外创建，交易时段再判断 |
| 行情新鲜度 | `market` 和即时成交类便捷动作必须使用 fresh quote；stale / missing quote 拒单 |
| 可成交性 | 买入需要卖盘可成交，卖出需要买盘可成交；盘口缺失时不能假装成交 |
| 费用 | 佣金双向收取，印花税仅卖出收取 |
| 现金 | 买入不能超过可用现金；挂买单冻结现金 |
| 持仓 | 卖出不能超过可卖数量；挂卖单冻结对应可卖数量 |

### 订单成交模拟

- `market` 订单用 fresh Quotes snapshot 的当前价和盘口模拟成交。
- `limit` 买单在 fresh quote 满足 `quote.price <= limitPrice` 且卖盘可成交时成交。
- `limit` 卖单在 fresh quote 满足 `quote.price >= limitPrice` 且买盘可成交时成交。
- stale / missing quote 不得触发成交；pending 订单保持 pending 并等待下一次 fresh quote。
- 盘口量不足时允许部分成交，剩余数量保持 pending。
- 过期订单变为 `expired`，并释放冻结现金 / 冻结持仓。

### 保护条件触发

| 条件 | 多头触发规则 | 结果 |
|---|---|---|
| 止损 | `price <= stopLoss` | `account-triggered(stop_loss)` |
| 止盈 | `price >= takeProfit` | `account-triggered(take_profit)` |
| 时间止损 | `now >= timeStopAt` | `account-triggered(time_stop)` |
| 失效条件 | 外部信号命中 | `account-triggered(invalidated)` |

触发后 Account 只记录和通知。Agent 可选择平仓、减仓、继续持有、调整保护条件或撤销挂单。

### 订阅集合

Account 对 Runtime Orchestrator 暴露当前关注集合：

```text
subscribed_codes = watchlist ∪ open_positions ∪ pending_orders
```

Runtime Orchestrator 将该集合合并核心指数后传给 Quotes refresh。Account 不直接维护行情源，也不调用 Quotes provider。

---

## 6. 验收标准 / 例子

- 前端 Account 读取只有 `fetch_account`；前端没有交易写 command。
- Agent Account 工具只有 `fetch_account` 和 `operate_account`。
- `operate_account(open_position)` 会创建订单；成交后生成 fill、position、account event，并刷新 snapshot。
- `operate_account(adjust_protection)` 只改保护条件，不自动创建卖单。
- `operate_account(open_position)` / `market` 订单在 quote stale 或缺失时必须拒单，不能依赖 Agent 自律。
- 价格触及止损 / 止盈时，Account 写 `AccountTrigger` 并 emit `account-triggered`，不调用 Agent、不自动平仓。
- `fetch_account({ include: { watchlist: true } })` 返回自选列表和 Quotes snapshot 行情摘要；缺行情时返回 warning。
- `AccountSnapshot` 的 cash / PnL / totalAssets 可由 events + fills + positions + Quotes snapshot 重算。
- Runtime Orchestrator 消费 `subscribed_codes()` 并注入 Quotes refresh；Account 不直接刷新行情。
- 当日买入 lot 的 `sellableQuantity` 为 0；次一交易日才可卖。
- 挂买单冻结现金，撤单 / 过期释放冻结现金。
- 挂卖单冻结对应可卖数量，撤单 / 过期释放冻结数量。
- Account 任一层不 import Agent / News 代码；只允许按架构规则读取 Quotes snapshot。

---

## 7. 模块边界外

这些能力不属于 Account 模块：

- 真券商交易。
- 投资观点生成。
- 新闻、研报、公告分析。
- 市场行情 provider。
- Agent run 调度。
- 自动决定止损 / 止盈触发后的交易动作。
