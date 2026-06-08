# Account 模块 Spec

> 本文档是 Account bounded context 的领域模型契约。模块边界 / 依赖方向以 `docs/design/architecture.md` 为准。
>
> 本文档只描述最新设计目标，作为实现依据。

## 一句话定位

**模拟券商账户 / 自动化交易执行模型**：Account 提供完整模拟账户能力，让自动化决策方像人使用交易软件一样管理自选、挂单、开仓、调仓、平仓和保护条件；对外读取本地账户状态。

Account 只负责交易执行、账户估值、事件审计和触发条件发现。投资判断、是否响应该触发、后续交易动作不属于 Account。

契约强度：

- `Order`、`TradeFill`、`Position`、`PositionLot`、`PositionProtection`、`AccountEvent`、`AccountTrigger`、`operate_account`、`update_watchlist` 是 `Spec-as-source`。
- 成交模拟、T+1、冻结现金 / 持仓、trigger 去重、事件先于状态是不变量。
- 费用参数、风控阈值默认值是 `Spec-anchored` 配置；运行时可覆盖，但缺省必须使用本文档默认值，执行时必须 fail closed。

共享类型见 [shared-types.md](shared-types.md)。

---

## 1. 责任边界

Account 负责：

- 维护模拟账户现金、订单、成交、仓位、自选和账户事件。
- 提供挂单、撤单、开仓、调仓、平仓、调整止损止盈 / 时间止损的能力。
- 管理自选列表，并提供带行情状态的自选列表读模型。
- 基于 Quotes snapshot 计算仓位价格、可卖数量、成本、盈亏、账户总资产。
- 提供订单成交条件和仓位保护条件的 evaluation use case。
- 条件触发后写账户事件并 emit 通知。
- 维护账户 snapshot、仓位列表、订单列表、自选列表读模型。

Account 不负责：

- 判断是否应该买入、卖出、加仓、减仓。
- 响应止损 / 止盈触发后的具体行为。
- 启动下游决策或管理跨模块调度。
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
| `AccountTrigger` | 订单或仓位条件触发后的通知事件 | `trigger_id` |

### 不变量

- Account 只有模拟账户语义，不连接真券商。
- 订单、仓位和保护条件写能力不通过人工 UI 暴露；交易意图只允许自动化决策方发起，`system` 仅允许写内部维护事实。
- 自选列表是非交易能力，可以由用户或自动化决策方维护。
- Account 可以读取 Quotes snapshot 做估值、成交模拟和触发判断，但不调用 Quotes provider。
- Account 任一层不 import 下游决策模块代码；触发条件只通过事件通知。
- 所有账户状态变化必须先写 `account_events`，再更新 / 派生订单、仓位、snapshot。
- 现金、PnL、总资产是派生值；可缓存 snapshot，但不得把缓存当真源。
- 订单和仓位分开建模：挂单是 `Order`，成交后才影响 `Position`。
- 止损 / 止盈不是默认真实挂单，而是 `PositionProtection` 条件；命中后只触发事件，由下游决策方决定后续动作。
- A 股交易规则由 Account 校验：整手、T+1、交易时段、可成交性、费用、印花税。
- 交易写路径必须检查 Quotes snapshot freshness；`quote.freshness.status == "stale"`、quote 缺失或关键价格缺失时，不允许即时成交。
- 批量读取返回 per-item warning/error；单个标的行情缺失不让整批失败。

### 订单模型

```ts
type TradingActor = "agent" | "system";
type AccountActor = TradingActor | "user";
type OrderStatus = "pending" | "partially_filled" | "filled" | "cancelled" | "rejected" | "expired";

type Order = {
  orderId: string;
  clientOrderId: string;
  tsCode: TsCode;
  side: "buy" | "sell";
  orderType: "market" | "limit";
  limitPrice?: Price;
  quantity: Shares;
  filledQuantity: Shares;
  status: OrderStatus;
  intent:
    | "open_position"
    | "scale_in"
    | "scale_out"
    | "close_position"
    | "direct_order";
  positionId?: string;
  reason?: string;
  actor: "agent";
  createdAt: OccurredAt;
  updatedAt: OccurredAt;
  expiresAt?: OccurredAt;
};
```

Actor 命名规则：

- `agent` 表示由 Agent tool / 后台自动化决策流程发起的交易意图；Account 只记录 actor，不依赖 Agent 代码。
- `agent` 交易写动作必须经由 Agent tool / 后台自动化流程进入，不通过人工 UI 暴露。
- `system` 只用于账户内部维护任务，例如订单过期、挂单成交评估、snapshot 重建和初始化；当前不允许创建 `Order`。
- `user` 只允许用于自选维护事件，不允许创建订单、调整仓位或调整保护条件。

字段说明：

| 字段 | 含义 | 规则 |
|---|---|---|
| `orderId` | 委托唯一 ID | 创建后不可变 |
| `clientOrderId` | 调用方生成的幂等键 | **在订单创建时随 `Order` 原子写入**（与建单同一事务，非事后盖戳），供反查与重复提交去重；同一 `clientOrderId` 只对应一个 `Order` |
| `tsCode` | 标准标的代码 | 必须是 Quotes 已知且 Account 可交易的 `TsCode` |
| `side` | 买入 / 卖出方向 | 买入冻结现金，卖出冻结可卖数量 |
| `orderType` | 市价 / 限价 | `market` 即时撮合：盘口量足够则全成交，量不足则按可成交量部分成交、剩余数量立即自动取消并入终态；不进入 `pending`。`limit` 可 pending |
| `limitPrice` | 限价价格 | `orderType = limit` 时必填 |
| `quantity` | 委托股 / 份数量 | 股票和场内基金必须 100 股 / 份整数倍 |
| `filledQuantity` | 已成交数量 | 由成交回报累加，不能超过 `quantity` |
| `status` | 订单状态 | 只能按订单状态机转换 |
| `intent` | 业务意图分类 | 用于审计和复盘，不替代 `side/orderType` |
| `positionId` | 关联仓位 | 加仓、减仓、平仓时必填；开仓成交后回填 |
| `reason` | 发起理由 | 写操作必须提供，用于审计 |
| `actor` | 发起者 | 当前订单只能由 `agent` 发起；`user` 和 `system` 都不允许创建订单 |
| `createdAt` / `updatedAt` | 创建 / 更新时间 | ISO-8601 |
| `expiresAt` | 订单过期时间 | 仅对 `limit` 有效；过期后释放冻结现金 / 持仓 |

规则：

- `open_position` / `scale_position` / `close_position` 可以作为写入口的便捷动作，但 Account 内部仍应落为订单 + 成交 + 仓位事件。
- `market` 表示用当前 fresh Quotes snapshot 模拟即时成交。盘口量足够时全部成交（`filled`）；盘口量不足时按可成交量部分成交、**剩余数量立即自动取消**进入终态（`partially_filled` 即为终态，同步 emit `order_cancelled` event 表达剩余量取消，且释放对应冻结资金 / 持仓）。**`market` 订单在任何情况下都不进入 `pending`**。
- `market` 订单不得携带 `limitPrice` 或 `expiresAt`；调用方传入时写入口必须返回 `accepted = false` / `invalid_input`，且不得创建 `Order`。
- `limit` 表示挂单；由定时任务根据 Quotes snapshot 判断是否成交、部分成交、过期。
- `limit` 订单可以在 quote stale 时创建为 pending，但不能在 stale quote 上成交。
- Account 交易只支持 `InstrumentCategory = "stock" | "fund"`；`index` 只能用于行情展示和市场背景，不能下单。
- 股票和场内基金买入 / 卖出数量必须是 100 股 / 份整数倍。
- 标的不存在返回 `not_found`；标的不是 `stock` / `fund` 返回 `instrument_not_tradable`；标的停牌或退市状态不可成交，使用 `instrument_suspended` 或 `instrument_not_tradable`。
- `Order.intent` 必须由写入口确定：
  - `place_order` -> `direct_order`，表示通用委托意图；`actor` 仍只能是 `agent`，不表示人工 UI 交易。
  - `open_position` -> `open_position`。
  - `scale_position(side = "increase")` -> `scale_in`。
  - `scale_position(side = "decrease")` -> `scale_out`。
  - `close_position` -> `close_position`。
- `direct_order` 是低阶委托语义：买入成交时若无 open position 则生成 `position_opened`，若已有 open position 则生成 `position_scaled`；卖出成交后若剩余持仓大于 0 则生成 `position_scaled`，若剩余为 0 则生成 `position_closed`。事件 payload 必须保留来源订单的 `intent = "direct_order"`。

订单创建评估和状态机：

| 当前阶段 / 持久状态 | 允许转换到 | 触发 |
|---|---|---|
| `creating` | `filled` | `market` 订单全量即时成交 |
| `creating` | `partially_filled` | `market` 订单按可成交量部分成交，剩余自动取消（终态） |
| `creating` | `rejected` | 参数、交易时段、行情、现金、持仓、风控等校验失败且订单已创建 |
| `creating` | `pending` | `limit` 订单创建并冻结资金 / 持仓成功 |
| `pending` | `partially_filled` | 挂单部分成交（剩余仍在 `pending`） |
| `pending` | `filled` | 挂单全部成交 |
| `pending` | `cancelled` | 显式撤单 |
| `pending` | `expired` | 到期未完全成交 |
| `partially_filled` | `filled` | 剩余数量继续成交完成（仅限 `limit`） |
| `partially_filled` | `cancelled` | 显式撤销剩余数量（仅限 `limit`） |
| `partially_filled` | `expired` | 剩余数量到期（仅限 `limit`） |

注意 `partially_filled` 在 `market` 与 `limit` 下语义不同：
- **`market`**：`partially_filled` 是终态，剩余量已自动取消并释放冻结；`order_cancelled` event 同步写入审计。
- **`limit`**：`partially_filled` 是中间态，剩余量仍 `pending`，可继续撮合、显式撤单或过期。

规则：

- `creating` 是创建命令内部的评估阶段，不是 `Order.status`，不得持久化或对外返回。
- `filled`、`cancelled`、`rejected`、`expired` 是终态，不能再转换。
- `market` 订单只能从 `creating` 到 `filled` / `partially_filled`（终态）/ `rejected`，不得持久化为 `pending`。`partially_filled` 时，剩余量必须在同一事务内自动 cancel 并释放冻结。
- `limit` 订单进入 `pending` 前必须完成冻结；冻结失败则拒绝，不得留下可成交挂单。
- `partially_filled` 的 `filledQuantity` 必须大于 0 且小于 `quantity`。

### 成交模型

```ts
type TradeFill = {
  fillId: string;
  orderId: string;
  positionId: string;
  tsCode: TsCode;
  side: "buy" | "sell";
  price: Price;
  quantity: Shares;
  commission: Money;
  stampTax: Money;
  transferFee: Money;
  occurredAt: OccurredAt;
};
```

字段说明：

| 字段 | 含义 | 规则 |
|---|---|---|
| `fillId` | 成交唯一 ID | append-only，不可变 |
| `orderId` | 来源订单 ID | 必须指向已接受订单 |
| `positionId` | 影响的仓位 ID | 每笔成交都必须绑定仓位；开仓首笔成交在同一事务内生成新仓位后写入该 ID |
| `tsCode` | 标的代码 | 与订单一致 |
| `side` | 成交方向 | 与订单方向一致 |
| `price` | 成交价 | 买入优先卖一价，卖出优先买一价 |
| `quantity` | 成交数量 | 不得超过订单剩余数量 |
| `commission` | 佣金 | 双向收取 |
| `stampTax` | 印花税 | 仅卖出收取 |
| `transferFee` | 过户费 | 仅 SH 市场 stock / fund 双向收取（按 `AccountFeePolicy.transferFeeRate` × notional）；SZ / BJ 为 0 |
| `occurredAt` | 成交时间 | 必须在可成交交易时段内 |

规则：

- 买入成交减少现金，卖出成交增加现金。
- 佣金双向收取，印花税仅卖出收取，过户费仅 SH 标的双向收取。
- 现金变动公式：
  - 买入：`cash -= price × quantity + commission + transferFee`
  - 卖出：`cash += price × quantity - commission - stampTax - transferFee`
- 买入成交生成新的 `PositionLot`，成交日当天不可卖。
- 卖出成交按可卖 lot 扣减，减少或关闭对应仓位。

### 持仓批次模型

`PositionLot` 是 T+1、可卖数量和挂卖冻结的最小重建单元。

```ts
type PositionLot = {
  lotId: string;
  positionId: string;
  tsCode: TsCode;
  sourceFillId: string;
  tradeDate: TradeDate;
  quantity: Shares;
  remainingQuantity: Shares;
  frozenQuantity: Shares;
  sellableFrom: TradeDate;
  createdAt: OccurredAt;
};
```

字段说明：

| 字段 | 含义 | 规则 |
|---|---|---|
| `lotId` | 批次唯一 ID | 由买入成交生成，创建后不可变 |
| `positionId` | 归属仓位 | 必须指向 open 或历史 position |
| `tsCode` | 标的代码 | 与成交 / 仓位一致 |
| `sourceFillId` | 来源买入成交 | 一个买入 fill 至少生成一个 lot |
| `tradeDate` | 买入成交交易日 | 用于 T+1 判断 |
| `quantity` | 批次原始数量 | 创建后不可变 |
| `remainingQuantity` | 未卖出数量 | 卖出成交后递减，不能小于 0 |
| `frozenQuantity` | 被挂卖单冻结的剩余数量 | 撤单 / 过期 / 成交后释放或扣减 |
| `sellableFrom` | 最早可卖交易日 | 股票和场内基金为买入成交的下一交易日 |
| `createdAt` | 批次创建时间 | 来源成交时间或事件 append 时间 |

规则：

- 买入成交必须生成 `PositionLot`；当日买入 lot 的 `sellableFrom` 必须是下一交易日。
- `remainingQuantity <= quantity`，`frozenQuantity <= remainingQuantity`。
- `Position.sellableQuantity = sum(max(remainingQuantity - frozenQuantity, 0))`，仅统计 `sellableFrom <= sellabilityTradeDate` 的 lot；`sellabilityTradeDate = MarketTimeContext.currentTradeDate ?? MarketTimeContext.latestCompletedTradeDate`。
- 卖出成交按可卖 lot FIFO 扣减，排序为 `sellableFrom asc, createdAt asc, lotId asc`。
- 挂卖单冻结同样按可卖 lot FIFO 分配；撤单 / 过期释放对应 lot 的 `frozenQuantity`。
- lot 是读模型的一部分，但必须能由 `TradeFill`、订单终态和冻结 / 释放事件重建。

### 仓位模型

```ts
type Position = {
  positionId: string;
  tsCode: TsCode;
  name: string;
  status: "open" | "closed";
  quantity: Shares;
  sellableQuantity: Shares;
  avgCost: Price;
  marketPrice?: Price;
  marketValue?: Money;
  quoteFreshness?: Freshness;
  realizedPnl: Money;
  unrealizedPnl?: Money;
  openedAt: OccurredAt;
  closedAt?: OccurredAt;
  protection?: PositionProtection;
  actor: "agent";
  reasoning?: string;
  warnings?: WarningCode[];
};
```

字段说明：

| 字段 | 含义 | 规则 |
|---|---|---|
| `positionId` | 仓位唯一 ID | 同一 `tsCode` 默认最多一个 open position |
| `tsCode` / `code` / `name` | 标的信息 | `tsCode` 是主键，`code/name` 用于展示 |
| `status` | 仓位状态 | 全部数量卖出后变为 `closed` |
| `quantity` | 当前持仓数量 | 由买卖成交派生 |
| `sellableQuantity` | 当前可卖数量 | 由 `PositionLot` + T+1 + 冻结数量派生 |
| `avgCost` | 平均成本 | 由成交和费用加权派生 |
| `marketPrice` / `marketValue` | 当前估值 | 来自 Quotes snapshot；缺行情可为空 |
| `quoteFreshness` | 估值行情新鲜度 | 来自 Quotes snapshot；缺行情时说明缺失原因 |
| `realizedPnl` | 已实现盈亏 | 来自历史卖出成交派生 |
| `unrealizedPnl` | 浮动盈亏 | 依赖 fresh / stale quote，可为空 |
| `openedAt` / `closedAt` | 开仓 / 平仓时间 | `closedAt` 仅 closed 仓位有值 |
| `protection` | 保护条件 | 止损止盈 / 时间止损，只触发事件不自动平仓 |
| `actor` | 初始开仓发起者 | 当前交易仓位只能由 `agent` 订单派生 |
| `reasoning` | 开仓理由摘要 | 来自外部决策方 thesis 或系统说明 |
| `warnings` | 仓位估值警告 | 行情缺失 / stale / 部分估值等 |

规则：

- `avgCost` 由买入成交价、买入佣金和剩余持仓数量加权派生。
- `sellableQuantity` 由 `PositionLot` 和 T+1 规则派生。
- `marketPrice`、`marketValue`、`unrealizedPnl` 来自 Quotes snapshot 派生。
- 同一 `ts_code` 默认只有一个 open position；新增买入成交若已有 open position，读模型合并为加仓。
- 高阶 `open_position(tsCode)` 如果该标的已有 open position，必须拒绝并返回 `invalid_input`；下游决策方需要显式调用 `scale_position(side = "increase")`，避免新的开仓理由被隐式挂到既有仓位上。
- **同一 `ts_code` 同时只允许一个未终态的 `open_position` 订单（`pending` 或 `partially_filled` 中的 limit 单都计入）**。第二次 `open_position(tsCode)` 在第一笔 limit 仍未终态时必须拒绝并返回 `invalid_input`（`field: "tsCode"`，附带 hint 指向首笔 pending order）。否则多笔 limit 开仓同时挂出后第二笔成交时会被合并为隐式加仓，违反"开仓理由不能隐式挂到既有仓位"的不变量。
- `Position.actor` 固定表示初始开仓发起者；后续调仓、平仓、保护条件调整由 `AccountEvent.actor` 审计，不回写为“当前管理者”。未来若支持系统导入持仓或系统再平衡，必须先扩展 Position actor 契约。
- 全部数量卖出后仓位关闭。

### 保护条件模型

```ts
type PositionProtection = {
  stopLoss?: Price;
  takeProfit?: Price;
  timeStopAt?: OccurredAt;
  invalidationSignals?: string[];
  enabled: boolean;
  revision: number;
  updatedAt: OccurredAt;
};
```

字段说明：

| 字段 | 含义 | 规则 |
|---|---|---|
| `stopLoss` | 止损触发价 | 多头仓位必须低于设置时参考价 |
| `takeProfit` | 止盈触发价 | 多头仓位必须高于设置时参考价 |
| `timeStopAt` | 时间止损点 | 到时只生成 trigger |
| `invalidationSignals` | 持仓 thesis 失效信号标签 | 由外部决策方显式写入；Account 不解析新闻或策略语义 |
| `enabled` | 是否启用保护 | false 时不触发 |
| `revision` | 保护条件版本 | 每次 `adjust_protection` 成功变更时递增 |
| `updatedAt` | 最近更新时间 | 调整保护条件时刷新 |

规则：

- 对多头仓位：`stopLoss < currentPrice`，`takeProfit > currentPrice`。
- 设置或调整价格型保护条件时必须有参考价：开仓即时成交携带的初始保护条件使用成交价；`adjust_protection` 使用 Quotes snapshot 当前价。参考价缺失返回 `quote_missing` / `quote_price_missing`；使用 stale quote 校验时允许写入，但响应必须带 `quote_stale` warning。
- 条件命中只生成 `AccountTrigger` 和 `AccountEvent`，不默认自动平仓。
- 下游决策方可在收到触发后调用 `operate_account` 平仓、减仓、撤单或调整保护条件。
- 同一保护条件连续命中时必须去重，避免同一价格 tick 反复触发下游响应。
- `invalidationSignals` 是可选的“持仓理由失效标签”集合。例如开仓理由依赖 `业绩修复`，下游决策方可以设置 `["earnings_recovery_failed"]`；当外部明确调用 `record_invalidation_signal(signal = "earnings_recovery_failed")` 时，Account 做精确匹配并生成 `invalidated` trigger。
- Account 不解析新闻、策略或自然语言，不判断信号是否成立；它只保存标签、记录外部显式 signal，并做字符串精确匹配。
- `adjust_protection.invalidationSignals` 采用全量替换语义：字段缺省表示不修改，传空数组表示清空；不得隐式 merge，避免旧失效条件残留。
- 同一 open position 的同一 `protectionRevision + signal` 只生成一次 `invalidated` trigger，即使该 trigger 已 handled 也不重复触发；需要再次触发时，下游决策方必须切换 signal 标签，或通过 `adjust_protection` 使 `revision` 递增。
- `adjust_protection` 对已存在 open position 是 upsert：没有 `PositionProtection` 时创建，有则更新；position 不存在或非 open 返回 `not_found` / `invalid_input`。
- 首次创建保护条件时，若请求未传 `enabled`，且至少设置了一个条件或失效信号，则 `enabled = true`；首次创建必须至少设置一个条件或失效信号，单独传 `enabled` 不构成有效保护条件，返回 `invalid_input`。
- `adjust_protection` 如果不会改变任何字段，必须返回 `accepted = false` / `invalid_input`，不得写空的 `protection_adjusted` 事件。
- 保护条件只能绑定到已存在的 open position；未成交的开仓限价单不能直接拥有 `PositionProtection`。
- `open_position` 如果使用 `market` 并即时成交，可以在生成 position 后立即应用请求携带的保护条件；如果使用 `limit` 进入 pending，则请求不得携带 `stopLoss` / `takeProfit` / `timeStopAt`，成交后由下游决策方通过 `adjust_protection` 设置。

止损 / 止盈与挂单的关系：

- `PositionProtection.stopLoss` / `takeProfit` 不是 `Order`，不进入 pending order 列表，也不冻结持仓。
- 保护条件是本地条件单语义：Account 定时评估行情，命中后只产生 `AccountTrigger` 和 `account-triggered` 事件。
- 触发后是否卖出、卖多少、用市价还是限价、是否继续持有，必须由 Agent 再调用 `operate_account` 决定。
- 如果后续要支持“触发后自动下单”的条件委托，必须新增独立 `ConditionalOrder` / 策略配置契约，不能复用 `PositionProtection` 偷偷自动成交。

### 自选模型

```ts
type WatchlistItem = {
  tsCode: TsCode;
  name?: string;
  addedAt: OccurredAt;
  note?: string;
};
```

字段说明：

| 字段 | 含义 | 规则 |
|---|---|---|
| `tsCode` / `code` / `name` | 自选标的 | Account 拥有列表，行情来自 Quotes |
| `addedAt` | 加入时间 | ISO-8601 |
| `note` | 备注 | 用户或外部决策方的观察理由；不用于交易判断 |

Account 拥有自选列表。Quotes 只提供自选标的的实时行情、涨跌幅、成交量、成交额等市场状态。`WatchlistItem` 不记录是谁添加的；添加 / 删除来源只进入对应 `AccountEvent.actor` 审计。

**持仓 ⊆ 自选不变量**：自选是跨局延续的「关注池」，持仓必属于关注池。

- **开仓即加自选**：任意 `open_position` 成交首次建仓时，该 `tsCode` 自动写入自选（已在则幂等跳过，不覆盖既有 `note`）。成功新增自选时写 `watchlist_added`，actor 跟随建仓 actor（`agent`）。
- **持仓必在自选内**：任何 open position 的 `tsCode` 一定能在自选列表中找到。
- **平仓不自动移除**：仓位关闭后该 `tsCode` 仍保留在自选；自选是关注池，不因平仓收缩，需要时由 `update_watchlist(remove)` 显式删除。

### 账户重置

**重置 = 重开一局模拟盘**。用户主动操作（带确认弹窗，避免误触），把账户财务侧清空回初始本金，旧局数据软归档可取回，**不动自选、不动 Agent 决策链**。

语义：

- **清账户财务侧**：现金回初始本金（`initial_cash`），清空持仓 / 挂单 / 成交 / lot / 保护条件 / 冻结 / 触发器 / 账户事件 / 当日权益基线 / `clientOrderId` 去重表。重置后账户读模型 = 空局。
- **软归档旧局（保留、可取回）**：重置时把旧局的账户摘要（局序号 / 起止时间 / 初始本金 / 期末现金 / 期末权益 / 已实现盈亏 / 成交笔数 / 平仓仓位数 / 事件数）作为单行 JSON 快照写入 `account_archive`，再清空 live 财务表。**不复制大表**——只存可读摘要，旧局逐笔明细不保留（设计取舍：模拟学习终端只需「上一局打成什么样」的复盘摘要，不需要逐单回放）。每局有递增的 `season`（局序号），归档按 `season` 取回。
- **保留自选**：`account_watchlist` 不清空，跨局延续（关注池语义）。重置后持仓为空，但「持仓 ⊆ 自选」不变量仍成立（空持仓平凡满足）。
- **不动 Agent 决策链**：`AgentRun` / `AnalysisResult` / `AgentTrade` / `orderId→runId` 索引 / 复盘报告均不属于 Account BC，重置不触碰；它们按 `runId` / 日期历史累积，跨局自然保留、可追溯。
- 重置写一条 `account_reset` 系统事件作为新局起点前的审计锚？—— 不写：`account_events` 被清空，重置审计落在 `account_archive`（旧局摘要 + `reset_at`）。重置后立即重新 `initialize_account_if_needed`，新局首事件仍是 `account_initialized`。

不变量：

- 重置后 `fetch_account` 的 positions / orders / fills / events / triggers 为空，`snapshot.cash == snapshot.totalEquity == initial_cash`。
- 自选在重置前后保持一致。
- 旧局可通过 `list_account_archives` 按 `season` 取回摘要。
- 重置只清 Account 财务侧；Agent 决策链记录在重置后仍可查询。

### 账户快照

```ts
type AccountSnapshot = {
  initialCash: Money;
  cash: Money;
  availableCash: Money;
  frozenCash: Money;
  marketValue: Money;
  totalAssets: Money;
  realizedPnl: Money;
  unrealizedPnl: Money;
  totalPnl: Money;
  pricedPositionCount: number;
  unpricedPositionCount: number;
  valuationFreshness: Freshness;
  openPositionCount: number;
  pendingOrderCount: number;
  capturedAt: OccurredAt;
  warnings?: WarningCode[];
};
```

字段说明：

| 字段 | 含义 | 规则 |
|---|---|---|
| `initialCash` | 初始资金 | 由 `account_initialized` 事件确定 |
| `cash` | 现金余额 | 由成交、费用、冻结释放派生 |
| `availableCash` | 可用现金 | `cash - frozenCash` |
| `frozenCash` | 挂买单冻结现金 | 撤单 / 过期 / 成交后释放或扣减 |
| `marketValue` | 当前持仓市值 | 基于 Quotes snapshot 派生 |
| `totalAssets` | 总资产 | `cash + marketValue` |
| `realizedPnl` | 已实现盈亏 | 已平仓 / 部分卖出成交派生 |
| `unrealizedPnl` | 未实现盈亏 | 当前持仓估值派生 |
| `totalPnl` | 总盈亏 | `realizedPnl + unrealizedPnl` |
| `pricedPositionCount` | 已成功估值仓位数 | 有可展示行情并计入 `marketValue` |
| `unpricedPositionCount` | 未成功估值仓位数 | 缺行情或行情不可用，未计入 `marketValue` |
| `valuationFreshness` | 账户估值新鲜度 | 由参与估值的 Quotes snapshot 派生。**`openPositionCount = 0` 时 `status = "fresh"`**（无仓位无估值需求，不会误导用户行情坏了）；有仓位时按子 quote freshness 聚合：全部 fresh → fresh；存在 stale 且无 missing → stale；存在 missing → missing |
| `openPositionCount` | 开仓中仓位数 | 查询派生 |
| `pendingOrderCount` | 未完成订单数 | 查询派生 |
| `capturedAt` | 快照生成时间 | 不是真源时间 |
| `warnings` | 账户估值警告 | 例如 `quote_missing` / `quote_stale` / `data_partial` |

### 账户估值和仓位价格计算

Account 拥有账户估值计算。Quotes 只提供标的价格、盘口、状态和 freshness；仓位成本、可卖数量、市值、盈亏和总资产都由 Account 基于账户事实派生。

规则：

- `avgCost` 由买入成交价、买入佣金和剩余持仓数量加权派生。
- `marketPrice` 使用 Quotes snapshot 的当前价；行情缺失时 `marketPrice`、`marketValue`、`unrealizedPnl` 可以为空，但仓位基础数量和成本仍必须返回。
- `marketValue = quantity * marketPrice`，仅在 `marketPrice` 存在时计算。
- `remainingCostBasis = quantity * avgCost`；`unrealizedPnl = marketValue - remainingCostBasis`，等价于 `(marketPrice - avgCost) * quantity`。未实现盈亏不预扣未来卖出佣金或印花税，仅在 fresh 或可展示的 stale quote 存在时计算；响应必须携带 quote freshness。
- `realizedPnl` 由卖出成交收入减去被卖出 lot 的成本、佣金、印花税和过户费派生。具体：`realizedPnl += (price - lotCost) × quantity - sellCommission - stampTax - sellTransferFee`。买入侧的佣金 / 过户费已计入 `lotCost`（通过 `avgCost`），不在卖出公式中重复扣减。
- `AccountSnapshot.marketValue` 只汇总成功估值的 open positions；缺行情或不可用行情的仓位不按 0 伪造估值，必须计入 `unpricedPositionCount`。
- `AccountSnapshot.totalAssets = cash + marketValue`；当 `unpricedPositionCount > 0` 时这是部分估值结果，`warnings` 必须包含 `data_partial`，不能被解释为完整账户净值。
- `AccountSnapshot.unrealizedPnl` 只汇总成功估值的 open positions；`totalPnl = realizedPnl + unrealizedPnl`。当 `unpricedPositionCount > 0` 时，`unrealizedPnl` 和 `totalPnl` 同样是部分估值结果，必须共用 `data_partial` warning，不能当作完整账户盈亏。
- `valuationFreshness.status` 规则：
  - `openPositionCount = 0` → `fresh`（无仓位无估值需求；不视为缺失）
  - `openPositionCount > 0` 且全部可估值且子 quote 全 fresh → `fresh`
  - `openPositionCount > 0` 且存在 stale 子 quote 且无 missing → `stale`
  - `openPositionCount > 0` 且存在 missing 子 quote（含完全无可估值）→ `missing`
- `cash`、`frozenCash`、`availableCash`、`totalAssets` 不得由前端或 Agent 自行计算后写回。
- Account 读取接口可以返回 stale quote 参与展示估值，但交易写路径必须 fail closed，不能用 stale / missing quote 成交。
- **账户是账户财务事实的单一所有者**：「日初权益」「当日组合收益率」「当日已平仓尾部连亏笔数」「当日组合回撤」都是**对账户财务事实的派生**，因此**归属本 BC**，由 Account 计算并通过下述只读 facade 暴露——消费方（Agent Runtime 复盘 / 风控编排，见 agent-runtime §3/§6）**不得自行从账户快照扒数据重算**这些财务派生，只能调 facade。Runtime 仅负责「何时调 + 跨 BC 编排」（如「超额 = 组合收益率 − 基准涨幅」是 Account facade 事实与 Quotes 事实的相减，留在 Runtime）。

### 账户财务事实只读 facade（账户为单一所有者）

`AccountSnapshot` 只表达**当前**权益（`totalAssets`）。但「当日基线 / 尾部连亏 / 当日回撤」等**当日财务派生**由 Account 持久化基线 + 计算并暴露为只读 facade：

| facade | 签名（概念） | 语义 |
|---|---|---|
| `day_open_equity(trade_date)` | `-> Option<Money>` | 该 CN 交易日的**日初权益基线**；当日尚未观测过则 `None` |
| `daily_return(now)` | `-> Option<f64>` | **当日组合收益率** =`(现权益 − 日初权益)/日初权益`；首次当日估值时幂等记录日初权益基线，之后用 `AccountSnapshot.totalAssets` 当现权益。日初为 0 或缺失 → `None` |
| `consecutive_losses(now)` | `-> u32` | 当日（CN 交易日）已平仓位中，从最近一笔起的**连续亏损笔数**（`realizedPnl < 0`）；按 `closedAt` 降序统计尾部连续为负的数量 |
| `daily_drawdown(now)` | `-> f64` | **当日组合回撤比例**（high-water-mark）=`(当日权益高水位 − 现权益)/当日权益高水位`，下限 0；当日权益高水位由 Account 持久化、随每次估值刷新，重启安全 |

规则：

- **日初权益基线幂等、重启安全**：以 CN 交易日为键，**首次当日观测权益时落库一条基线**（`INSERT OR IGNORE`），同一交易日不被后续观测覆盖；进程重启后读已有基线。当日权益高水位（drawdown 用）同样持久化，每次估值取 `max(已存高水位, 现权益)`。
- 现权益统一取 `AccountSnapshot.totalAssets`（账户自有估值，部分估值时带 `data_partial` warning，facade 不另造数字）。
- 这些 facade 是**只读 / 幂等观测**：`daily_return` / `daily_drawdown` 在首次观测时落基线属内部维护写（`actor = system` 语义，不产生投资意图、不进 `maxDailyNewOrders`），不写 `account_events`（基线表是辅助派生表，不是账户状态真源；账户读模型仍可由事件 + 行情完整重建）。
- **持久化归属 Account**：基线表 `account_day_equity`（schema 属 account，读写在 `AccountRepository`/`AccountService`）。其 migration 因 `lib.rs` 全局拼接 append-only 约束被物理放到拼接末尾（见 lib.rs migration 顺序注释 + `account_migrations_tail()`）；**migration 的物理位置 ≠ 表的逻辑归属**——表仍是 Account 拥有的财务事实。

### 账户事件模型

`account_events` 是 Account 状态变化真源。订单、成交、仓位、lot、保护条件、自选和 snapshot 都必须可由事件和当前行情重建。

```ts
type AccountEventType =
  | "account_initialized"
  | "order_placed"
  | "order_cancelled"
  | "order_rejected"
  | "order_expired"
  | "order_partially_filled"
  | "order_filled"
  | "position_opened"
  | "position_scaled"
  | "position_closed"
  | "protection_adjusted"
  | "watchlist_added"
  | "watchlist_removed"
  | "watchlist_note_updated"
  | "cash_frozen"
  | "cash_released"
  | "shares_frozen"
  | "shares_released"
  | "invalidation_signal_recorded"
  | "trigger_created"
  | "trigger_handled"
  | "snapshot_rebuilt";

type AccountEvent = {
  eventId: string;
  eventType: AccountEventType;
  orderId?: string;
  fillId?: string;
  positionId?: string;
  tsCode?: TsCode;
  reason?: string;
  actor: AccountActor;
  payload: JsonValue;
  occurredAt: OccurredAt;
};
```

字段说明：

| 字段 | 含义 | 规则 |
|---|---|---|
| `eventId` | 账户事件唯一 ID | append-only，不复用 |
| `eventType` | 事件类型 | 只表达 Account 内发生的事实 |
| `orderId` / `fillId` / `positionId` | 关联对象 | 事件涉及对象时必须填写 |
| `tsCode` | 关联标的 | 标的相关事件必须填写 |
| `reason` | 触发理由 | 来自 command reason 或系统说明 |
| `actor` | 事件发起者 | `agent` = Agent / 自动化决策方调用；`system` = 调度 / 重建 / 过期处理；`user` = 自选维护 |
| `payload` | 重放所需事实 | 最小可重建数据，不放展示冗余 |
| `occurredAt` | 事件发生时间 | 审计排序依据 |

规则：

- 所有账户状态变化必须先 append `AccountEvent`。
- 账户事件流的首个账户事实必须是 `account_initialized`；其 payload 至少包含 `initialCash`，重建时不得从可变运行时配置隐式改变初始资金。
- `account_initialized` 只能由 `system` actor 在 application 启动或账户首次使用时通过内部 `initialize_account_if_needed` 写入一次；`operate_account` / `update_watchlist` 不提供初始化 action。
- `initialize_account_if_needed` 必须幂等：已存在 `account_initialized` 且 `initialCash` 相同则不写新事件；若已有账户但请求的 `initialCash` 不同，必须 fail closed 并返回 `invalid_input` 或 `db_error`。
- `payload` 保存足以重放读模型的最小事实，不保存 UI 展示冗余。
- 拒单若订单已经创建，必须写 `order_rejected`；参数校验在订单创建前失败时可以只返回 rejection response。
- 创建 `AccountTrigger` 时必须有可追溯的 `AccountEvent`。订单型 trigger 的 `eventId` 固定指向对应订单终态事件：`order_filled` / `order_rejected` / `order_expired`；保护条件和失效信号 trigger 必须写 `trigger_created` 事件，并使用该 `trigger_created.eventId`。
- `order_cancelled` 和 `order_partially_filled` 只产生账户更新，不创建 `AccountTrigger`。

### 冻结和重建规则

冻结现金 / 持仓是 pending 订单的派生约束，不是独立真源。`cash_frozen`、`cash_released`、`shares_frozen`、`shares_released` 是审计事件；重建时必须能由订单、成交、lot 和这些事件校验一致性。

规则：

- `limit` 买单进入 `pending` 前必须冻结预计最大占用现金：`limitPrice * remainingQuantity + estimatedFees`。
- accepted `limit` 订单进入 `pending` 时，同一事务内事件 append 顺序必须稳定：先写 `order_placed`，再写 `cash_frozen` 或 `shares_frozen`；若冻结失败，事务不得提交 `order_placed`。
- `market` 买单即时成交，不保留长期冻结；若成交前需要内部冻结，必须在同一写事务内释放或扣减。
- 买单部分成交时，成交部分转为实际现金扣减；未成交部分继续冻结，若实际成交价低于冻结价，差额必须释放。
- 买单撤单 / 过期时，释放该订单剩余未成交数量对应的冻结现金。
- `limit` 卖单进入 `pending` 前必须从可卖 lot 中冻结对应数量；冻结失败返回 `insufficient_sellable_quantity`。
- 卖单部分成交时，成交部分从 frozen lot 转为 sold；未成交部分继续冻结。
- 卖单撤单 / 过期时，释放该订单剩余未成交数量对应的 frozen lot。
- `AccountSnapshot.frozenCash` 由未完成买单剩余冻结金额派生；`Position.sellableQuantity` 必须扣除 frozen lot。
- 重建读模型时，若冻结事件和订单 / lot 派生结果不一致，必须 fail closed，返回 `duplicate_event` 或 `db_error`，并记录可观测日志；不能静默修正现金或持仓。

### 触发事件模型

`AccountTriggerType` 见 [shared-types.md](shared-types.md)（跨 BC 单一定义）。各取值语义：`stop_loss` / `take_profit` / `time_stop` 为保护条件命中；`order_filled` / `order_rejected` / `order_expired` 为订单终态事实通知；`invalidated` 为命中显式失效信号。

```ts
type AccountTrigger = {
  triggerId: string;
  triggerType: AccountTriggerType;
  orderId?: string;
  positionId?: string;
  tsCode?: TsCode;
  price?: Price;
  threshold?: Price | OccurredAt | string;
  quoteFreshness?: Freshness;
  warnings?: WarningCode[];
  eventId: string;
  handled: boolean;
  occurredAt: OccurredAt;
};

type MarkTriggerHandledResponse =
  | {
      accepted: true;
      trigger: AccountTrigger;
      accountEventIds: string[];
    }
  | {
      accepted: false;
      reason: ErrorCode;
      message?: string;
    };
```

字段说明：

| 字段 | 含义 | 规则 |
|---|---|---|
| `triggerId` | 跨模块触发幂等 ID | Agent Runtime / 下游决策方用它去重 |
| `triggerType` | 触发类型 | 止损、止盈、订单结果等 |
| `orderId` / `positionId` | 关联订单 / 仓位 | 按触发来源填写 |
| `tsCode` | 关联标的 | 标的相关 trigger 必须填写 |
| `price` | 触发时价格 | 价格型触发必须保存 |
| `threshold` | 触发阈值 | 例如止损价、止盈价、时间止损点 |
| `quoteFreshness` | 触发所用行情新鲜度 | 价格型触发必须填写；非价格型可为空 |
| `warnings` | 触发警告 | 例如使用 stale quote 触发通知 |
| `eventId` | 对应 AccountEvent | trigger 必须可追溯到账户事件 |
| `handled` | 是否被下游确认处理 | 不代表已交易，只代表不再重复路由 |
| `occurredAt` | 触发时间 | 用于审计；trigger 幂等以 `triggerId` 稳定键为准 |

规则：

- `triggerId` 是跨模块幂等键。
- `handled` 表示 Agent Runtime 已确认该 trigger 被下游消费完成或按策略显式忽略；不表示下游决策方采取了交易动作。
- Account 只保存 `handled` 最终确认位；Agent Runtime 的 consumption record 是 `processing` / `failed` / retry 的权威状态。Agent run 启动成功不等于 handled，只有 run 完成消费、显式忽略或不可恢复放弃后才允许调用 `mark_trigger_handled`。
- 同一保护条件按下方 `triggerId` 确定性键只能生成一个 trigger。
- `triggerId` 必须按稳定字段确定性生成：
  - 价格型持仓保护：`triggerType + positionId + tsCode + protectionRevision + threshold + tradeDate`。
  - 时间止损：`triggerType + positionId + tsCode + protectionRevision + timeStopAt`。
  - 失效信号：`triggerType + positionId + tsCode + protectionRevision + signal`。
  - 订单终态：`triggerType + orderId + tsCode + 对应终态 AccountEvent.eventId`。
  盘中连续 tick、分页重试和进程重启不得重复生成相同 trigger。
- 价格型持仓保护键中的 `tradeDate = MarketTimeContext.currentTradeDate ?? MarketTimeContext.latestCompletedTradeDate`；盘后评估仍使用当日交易日，周末 / 节假日使用最近已完成交易日。
- 同一 `protectionRevision`、同一阈值、同一交易日的价格型保护 trigger 最多生成一次，即使已 handled 也不在同日重复生成；下游如果希望再次触发，必须调整保护条件使 `revision` 递增，或等待下一交易日。
- `mark_trigger_handled(trigger_id, reason)` 是内部维护 API，用于 Agent Runtime 确认下游消费完成、显式忽略或不可恢复放弃后确认 trigger 不再需要重复路由。
- `mark_trigger_handled` 必须幂等：已 handled 的 trigger 再次标记仍返回同一 trigger，不重复写事件。
- 首次标记 handled 必须写 `trigger_handled` 事件，并将 `AccountTrigger.handled` 置为 true；未知 `trigger_id` 返回 `accepted = false` / `not_found`。

触发评估返回：

```ts
type AccountTriggerResult = {
  triggers: AccountTrigger[];
  accountEventIds: string[];
  hasMore: boolean;
  nextCursor?: string;
  warnings?: WarningCode[];
};
```

规则：

- `triggers` 只包含本批次新创建或新命中的 trigger；重复命中的既有 trigger 不重复返回。
- `accountEventIds` 顺序必须等于事件 append 顺序。
- `hasMore = true` 时必须返回 `nextCursor`，Agent Runtime 必须使用它继续调度；Account 不在单个 tick 内无限循环。
- `nextCursor` 必须基于持久排序键生成，例如 `phase + updatedAt/createdAt + orderId/positionId`，不得使用内存 offset；它必须可跨进程重启后恢复同一批次之后的扫描位置。

### 硬风控模型

```ts
type AccountFeePolicy = {
  commissionRate: Ratio;
  minCommission: Money;
  stampTaxSellRate: Ratio;
  transferFeeRate?: Ratio;
};

type AccountRiskPolicy = {
  maxSinglePositionRatio: Ratio;
  maxGrossExposureRatio: Ratio;
  maxOrderValueRatio: Ratio;
  maxDailyNewOrders: number;
};
```

默认规则：

- 缺省费用参数：`commissionRate = 0.0003`（万 3，双向）、`minCommission = 5`（最低 5 元）、`stampTaxSellRate = 0.0005`（千五，仅卖）、`transferFeeRate = 0.00001`（A 股沪市过户费 0.001%，双向；SZ / BJ 不收，由 adapter 按 `TsCode.market` 决定是否乘入）。
- 费用计算公式：
  - `commission = max(notional × commissionRate, minCommission)`，双向。
  - `stampTax = notional × stampTaxSellRate`，仅 `side = "sell"`。
  - `transferFee = notional × transferFeeRate`，仅 `TsCode.market = "SH"` 且 `InstrumentCategory ∈ {stock, fund}`；其他市场为 0。
  - `notional = price × quantity`。
- 缺省风控阈值：`maxSinglePositionRatio = 0.25`，`maxGrossExposureRatio = 0.95`，`maxOrderValueRatio = 0.25`，`maxDailyNewOrders = 20`。
- 自动化交易写动作必须携带可审计 reason；是否存在 active strategy 由下游决策纪律保证，不属于 Account 依赖。
- 风控估值不得直接使用部分估值的 `AccountSnapshot.totalAssets` 做分母。买入风控必须计算独立的 `riskEquity = cash + sum(positionRiskValue)`：已估值仓位用 `marketValue`，未估值仓位用 `remainingCostBasis`；任何仓位不得按 0 计入风险敞口。
- 单票和总仓位风控中的新增买入价值按订单最大占用计算：`market` 用 fresh quote 的预计成交价，`limit` 用 `limitPrice`。
- 风控敞口必须包含 active buy orders 的剩余最大占用：`pending` / `partially_filled` 买单按剩余数量和订单价格计入对应标的与总敞口；不能只统计已成交持仓。
- 任何买入后单票市值超过 `maxSinglePositionRatio` 必须拒绝。
- 任何买入后总仓位超过 `maxGrossExposureRatio` 必须拒绝。
- 单笔订单金额超过 `maxOrderValueRatio * riskEquity` 必须拒绝。
- `maxDailyNewOrders` 按 `Asia/Shanghai` 自然日统计 `actor = agent` 新创建订单数；撤单 / 过期不扣减，跨日遗留 pending 订单不计入新一天，`system` 维护动作不计入。
- 当前 `system` 不允许发起投资意图，故不计入 `maxDailyNewOrders`；未来若引入 system-initiated rebalancing，必须新增 actor / policy 并纳入明确风控计数，不能复用 maintenance 语义绕过限制。
- 风控拒绝使用 `risk_limit_exceeded`，不得创建成交。

### Actor 语义

Account 中的 `actor` 表示账户写动作或事件的发起者，不表示行情 / 新闻 provider 来源。

| actor | 含义 | 允许场景 |
|---|---|---|
| `agent` | Agent / 外部自动化决策方调用 Account 写入口 | 下单、撤单、调整保护条件、维护自选 |
| `system` | 系统内部维护动作 | 订单过期、成交评估、snapshot 重建、初始化 |
| `user` | 用户发起的非交易账户维护动作 | 仅添加 / 删除自选、更新自选备注 |

规则：

- 人工 UI 不允许发起交易写动作；`user` actor 不得用于订单、仓位、保护条件或成交事件。
- `system` 不代表投资判断，不能主动创建新的开仓 / 加仓 / 减仓 / 平仓意图；它只用于初始化、订单过期、挂单成交评估、冻结释放和 snapshot 重建等维护事实。
- **`operate_account` 即时市价成交**（同步路径）产生的 `order_placed` / `order_filled` / `order_partially_filled` / `position_*` 事件 `actor = agent`——它们是 agent 调用的直接同步结果，审计链溯源到本次 `operate_account`。唯一例外：市价单未成交剩余量的 `order_cancelled`（reason=`market_remainder_auto_cancel`）是系统机械撤单（市价单不挂单留存），`actor = system`。
  - 对比：**限价单挂单后由调度评估成交**的终态事件（`order_filled` / `order_partially_filled` / `order_cancelled` / `order_expired`）`actor = system`——成交发生在 agent 调用之外、由后台撮合评估触发。
- 自选维护是非交易能力，允许 `user` / `agent` / `system` 发起。
- `actor` 必须进入 `AccountEvent`，用于审计链追踪。

---

## 3. 数据流

### 写入流

```text
account write request (operate_account / update_watchlist)
  -> account command DTO
  -> Account use case
  -> domain rule validation
  -> append account_events
  -> create AccountTrigger for order terminal events when applicable
  -> upsert orders / fills / positions / lots or watchlist
  -> rebuild account snapshot
  -> emit account-updated
  -> emit account-triggered when new AccountTrigger exists
```

规则：

- 所有写操作串行化，避免现金、仓位、订单并发漂移。
- `operate_account` 交易意图必须有 `actor = agent` 和可审计 note/reason；`system` 只用于内部维护事件，不创建投资意图。
- 失败的写操作也应能返回明确 rejection reason；是否写 rejection event 由订单是否已创建决定。
- `account-updated` payload 使用 [shared-types.md](shared-types.md) 定义的 `AccountUpdatedPayload`，至少包含本次 append 的 `accountEventIds` 和重建后的 `snapshotCapturedAt`；消费者收到后应按需重新读 `fetch_account`。
- `order_filled`、`order_rejected`、`order_expired` 是订单终态通知事实；当这些事件被 append 时，Account 必须在同一事务内创建对应 `AccountTrigger` 并 emit `account-triggered`。这包括 `market` 订单在 `operate_account` 内即时成交 / 拒绝，也包括定时评估 pending 订单后的成交 / 过期。
- 参数校验在订单创建前失败时没有订单事实，不写 `order_rejected`，也不创建 order trigger；调用方只收到 rejected response。
- `order_cancelled` 由调用方显式发起，只 emit `account-updated`，不 emit `account-triggered`。

### 读取流

```text
external read request
  -> account query facade
  -> account tables + ACCOUNT_SNAPSHOT
  -> optional Quotes snapshot join for market fields
  -> response with freshness/warnings
```

规则：

- 读取 Account 不触发远端行情 provider。
- 仓位和自选的行情字段只从 Quotes snapshot 左连接。
- Quotes snapshot 缺失时返回 `quote_missing` warning，账户基础数据仍返回。

### 触发评估流

```text
external scheduler tick
  -> pending orders + open positions
  -> Quotes snapshot
  -> simulate order fills / expirations
  -> evaluate PositionProtection
  -> append AccountEvent / AccountTrigger
  -> emit account-triggered / account-updated
```

规则：

- Account 只判断条件和发事件，不调用下游决策模块。
- `account-triggered` 是通知，不是交易指令；payload 使用 [shared-types.md](shared-types.md) 定义的 `AccountTriggeredPayload`。
- `account-updated` payload 使用 `AccountUpdatedPayload`；触发评估若同时产生 trigger 和账户读模型变化，两个事件必须引用同一批 `accountEventIds`。
- 触发事件必须带 `trigger_id`，供下游消费者做幂等处理。
- 订单终态 trigger 和保护条件 trigger 都由 Account 创建；Agent Runtime 只消费 `account-triggered` 并路由，不补造 trigger。

---

## 4. 对外接口

### 读取接口

Account 对外暴露一个读取 command：

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
  positionStatus?: "open" | "closed" | "all";
  orderActive?: boolean;
  orderStatusIn?: OrderStatus[];
  clientOrderId?: string;
  triggerHandled?: boolean | "all";
  limit?: number;
  offset?: number;
};

type FetchAccountResponse = {
  snapshot?: AccountSnapshot;
  positions?: Position[];
  orders?: Order[];
  watchlist?: Array<WatchlistItem & {
    quote?: {
      price?: Price;
      changePercent?: Percent;
      volume?: Volume;
      amount?: Amount;
      source?: string;
      freshness?: Freshness;
    };
  }>;
  events?: AccountEvent[];
  triggers?: AccountTrigger[];
  warnings?: WarningCode[];
};
```

约束：

- `positionStatus` 只过滤 `positions`，默认 `open`。
- `orderActive` 是订单过滤关键字，不是 `Order.status`；`true` 表示 `pending + partially_filled`，`false` 表示非活跃订单。`include.orders = true` 且未传 `orderStatusIn` / `orderActive` 时，默认 `orderActive = true`。
- `orderStatusIn` 只接受真实 `OrderStatus[]`，用于精确过滤订单状态；如果同时传 `orderStatusIn` 和 `orderActive`，必须取两者交集。
- `clientOrderId` 只过滤 `orders`，按幂等键精确反查已有 `Order`（崩溃恢复对账用）；命中时返回对应单条订单，未命中返回空。
- `triggerHandled` 只过滤 `triggers`；`false` 表示未处理 trigger，`true` 表示已处理 trigger，`"all"` 表示不过滤。`include.triggers = true` 且未传时，默认 `false`。
- `limit` 默认 100，最大 500；`offset` 默认 0。分页应用到 `positions`、`orders`、`events`、`triggers` 这些可增长集合；`snapshot` 和 `watchlist` 不分页。
- 人工 UI 不能通过 Tauri command 做交易写操作。
- `operate_account` 只允许 Agent tool / 外部自动化决策运行时调用；`system` 维护流程不通过该入口创建订单，前端不能绕过 Agent 下单。
- 前端可以通过非交易写入口 `update_watchlist` 添加 / 删除自选或更新备注。
- 展示自选时，Account 返回自选元信息，行情字段来自 Quotes snapshot。
- `fetch_account` 是统一读取 facade；前端或 Agent 可以通过 `include.watchlist`、`include.positions`、`include.snapshot` 获取自选、仓位和账户总览。实现可以提供轻量 wrapper，但不得绕过同一套 Account query 规则。

### 写入接口

#### `operate_account`

```ts
type OperateAccountInput =
  | {
      action: "place_order";
      clientOrderId: string;
      tsCode: TsCode;
      side: "buy" | "sell";
      orderType: "market" | "limit";
      limitPrice?: Price;
      quantity: Shares;
      expiresAt?: OccurredAt;
      reason: string;
    }
  | {
      action: "cancel_order";
      clientOrderId: string;
      orderId: string;
      reason: string;
    }
  | {
      action: "open_position";
      clientOrderId: string;
      tsCode: TsCode;
      quantity: Shares;
      orderType?: "market" | "limit";
      limitPrice?: Price;
      expiresAt?: OccurredAt;
      stopLoss?: Price;
      takeProfit?: Price;
      timeStopAt?: OccurredAt;
      reason: string;
    }
  | {
      action: "scale_position";
      clientOrderId: string;
      positionId: string;
      side: "increase" | "decrease";
      quantity: Shares;
      orderType?: "market" | "limit";
      limitPrice?: Price;
      expiresAt?: OccurredAt;
      reason: string;
    }
  | {
      action: "close_position";
      clientOrderId: string;
      positionId: string;
      quantity?: Shares;
      orderType?: "market" | "limit";
      limitPrice?: Price;
      expiresAt?: OccurredAt;
      reason: string;
    }
  | {
      action: "adjust_protection";
      positionId: string;
      stopLoss?: Price | null;
      takeProfit?: Price | null;
      timeStopAt?: OccurredAt | null;
      invalidationSignals?: string[];
      enabled?: boolean;
      reason: string;
    }
  | {
      action: "record_invalidation_signal";
      positionId: string;
      signal: string;
      evidenceRef?: string;
      reason: string;
    };
```

约束：

- accepted action 和已经产生副作用的 rejected action 都必须写入可审计事件；参数预校验在任何账户事实创建前失败时，可以只返回 rejected response，并令 `accountEventIds = []`。
- `open_position`、`scale_position`、`close_position` 是便捷交易动作，内部仍走订单 / 成交流。
- `open_position` 必须只用于建立新标的仓位；如果 `tsCode` 已存在 open position，必须返回 `accepted = false` / `invalid_input`。加仓必须显式使用 `scale_position(side = "increase")`。
- `place_order` 是低阶委托入口，不做开仓 / 加仓 / 平仓意图推断；成交后按成交事件 append 时刻的账户状态派生 `position_opened` / `position_scaled` / `position_closed` 事件。
- 同一 `tsCode` 存在多个 pending buy order 时，派生以成交事件 append 顺序为准：第一笔成交若当时没有 open position 则 `position_opened`，后续成交若当时已有 open position 则 `position_scaled`。所有写操作串行化；同一批次内排序相同时以 `orderId` 作为稳定 tie-breaker。
- `cancel_order` 只允许撤销 `pending` / `partially_filled` 订单；未知订单返回 `not_found`，终态订单返回 `order_not_pending`。撤销部分成交订单只释放剩余未成交数量对应的冻结现金 / 持仓，不回滚已成交部分。
- `scale_position.side = "increase"` 内部生成买入 / 加仓订单，`side = "decrease"` 内部生成卖出 / 减仓订单；`quantity` 必须为正数，不能用负数表达方向。
- `scale_position.side = "decrease"` 默认只做部分减仓；`quantity` 必须满足 `0 < quantity < position.quantity`。若 `quantity` 等于全部持仓，返回 `accepted = false` / `invalid_input`，应改用 `close_position`；若 `quantity > position.quantity` 或 `quantity > sellableQuantity`，返回 `accepted = false` / `insufficient_sellable_quantity`。
- `close_position.quantity` 语义：
  - **缺省**：尝试卖出**当前可卖部分**——实际下单数量 = `min(position.quantity, sellableQuantity)`。若 `sellableQuantity < position.quantity`（T+1 锁仓未到次日），不报错；实际下单 `sellableQuantity`，剩余持仓保留，并在响应 `warnings` 中加 `data_partial` (`field: "quantity"`)；只有当 `sellableQuantity == 0` 时返回 `insufficient_sellable_quantity`。
  - **显式传入**：必须 `0 < quantity <= position.quantity`，否则 `invalid_input` 并提示使用 `scale_position(side = "decrease")`。若 `quantity > sellableQuantity`，返回 `insufficient_sellable_quantity`（显式数量要求精确，不做隐式裁剪）。
  - 设计意图：缺省语义对应"卖能卖的"（UX 友好）；显式语义对应"我要卖这么多"（精确，不允许隐式部分）。
- `adjust_protection` 只调整保护条件，不直接下单。
- `adjust_protection` 字段缺省表示“不修改该字段”；`stopLoss` / `takeProfit` / `timeStopAt` 传 `null` 表示清除该条件，传具体值表示设置或替换；`enabled` 缺省表示不修改，传 `true` / `false` 表示启用 / 禁用整组保护条件。
- `open_position` / `scale_position` / `close_position` 的 `orderType` 缺省时必须按 `"market"` 处理；`place_order` 必须显式传 `orderType`。
- `limit` 订单必须提供 `limitPrice > 0`；`market` 订单不得提供 `limitPrice`。`expiresAt` 若显式传入，必须晚于当前 `now`。
- 所有 `limit` 订单（含 `place_order` 和便捷交易动作）可以设置 `expiresAt`；未设置时按当日有效委托处理：若当前自然日是交易日且当前时间早于 15:00 Asia/Shanghai，默认 `expiresAt = MarketTimeContext.currentTradeDate 15:00 Asia/Shanghai`；若当前不在交易日或当前时间已达到 / 晚于 15:00，默认 `expiresAt = MarketTimeContext.nextTradeDate 15:00 Asia/Shanghai`。
- `open_position` 使用 `limit` 且可能进入 pending 时，不允许同时携带 `stopLoss` / `takeProfit` / `timeStopAt`，避免给未存在的仓位设置保护条件；成交后由下游决策方通过 `adjust_protection` 设置。
- `record_invalidation_signal` 只记录外部显式信号；调用时必须先写 `invalidation_signal_recorded` 事件。无论保护条件是否启用都要记录该事件；只有当 `enabled = true` 且 `signal` 精确命中该仓位当前 `invalidationSignals` 时，才生成 `invalidated` trigger，不自动交易。
- `record_invalidation_signal` 只允许作用于 open position；position 不存在返回 `not_found`，position 已关闭返回 `invalid_input`，且不得写 `invalidation_signal_recorded`。
- 返回必须包含 `accepted` 布尔值、相关订单或仓位 ID、错误原因和最新 snapshot 摘要。
- 交易 action 必须先校验标的可交易性；非股票 / 场内基金返回 `instrument_not_tradable`。
- 即时成交类 action 遇到 stale / missing quote 必须返回 `accepted = false`，reason 使用 `quote_stale` / `quote_missing` / `quote_price_missing`。
- 交易写类 action（`place_order` / `cancel_order` / `open_position` / `scale_position` / `close_position`）必须携带调用方生成的 `clientOrderId` 幂等键；watchlist 类写入口（`update_watchlist`）不需要。
- **`clientOrderId` 重复提交去重**：同一 `clientOrderId` 再次提交时，必须返回首次提交的结果（同一 `orderId` / `fillIds` / `accountEventIds` / `snapshot`），不重复创建 `Order` / `TradeFill` / 账户事件。重复提交不报错，等同回放首单结果；这是 `duplicate_event` 错误的对应"安全态"——去重命中返回首单结果，而不是返回 `duplicate_event`。
- **按 `clientOrderId` 反查已有 `Order`**：`fetch_account` 的订单查询支持 `clientOrderId` 过滤（见 `FetchAccountRequest.clientOrderId`），调用方据此对账已提交的命令是否落库。
- **去重键持久化**：`clientOrderId` 到首单结果的映射必须随订单一起持久化，跨进程重启仍生效；Agent Runtime 崩溃恢复时靠它重放命令而不产生重复账户事实。
- **`clientOrderId` 与 `Order` 原子写入**：`clientOrderId` 必须在创建 `Order` 的同一事务里写入，不允许"先建单、后另起一步盖戳"的两步式写法——否则"已建单但未盖戳"的崩溃窗口会让按 `clientOrderId` 反查漏命中，恢复对账退化成"猜失败"。配套提供按 `clientOrderId` 精确反查单条 `Order` 的只读能力，供 Runtime 启动恢复对账。

响应契约：

```ts
type OperateAccountResponse = {
  accepted: boolean;
  reason?: ErrorCode;
  message?: string;
  clientOrderId?: string;
  orderId?: string;
  fillIds?: string[];
  positionId?: string;
  triggerId?: string;
  rejectionEventId?: string;
  accountEventIds: string[];
  snapshot: AccountSnapshot;
  warnings?: WarningCode[];
};
```

规则：

- `accepted = true` 表示 Account 接受命令并创建了预期账户事实；`limit` 订单可能仍是 pending。
- `accepted = false` 表示 Account 拒绝该命令；若拒绝发生在订单事实创建之后，响应可以携带 `orderId`、`accountEventIds` 和对应 order trigger，但订单状态必须为 `rejected`。
- `rejectionEventId` 仅在写入 `order_rejected` 事件时返回，且必须属于 `accountEventIds`。
- `market` 订单 `accepted = true` 时必须已经成交；`accepted = false` 时可以没有订单事实，或有明确 `order_rejected` 事件，但绝不能返回 pending。
- rejected 响应必须有 `reason`。
- 有副作用的 rejected 操作必须返回对应 `accountEventIds`。
- `accountEventIds` 的顺序必须等于事件 append 顺序，供审计链展示。
- 交易写类 action 必须回显本次 `clientOrderId`；重复提交命中去重时回显的也是该幂等键，且其余字段与首单结果一致。

字段矩阵：

| 场景 | `accepted = true` 必须返回 | 说明 |
|---|---|---|
| `place_order` / `open_position` / `scale_position` / `close_position` 创建订单 | `orderId`、`accountEventIds`、`snapshot` | `limit` pending 也必须返回 `orderId`，供 Runtime 建立订单反查索引 |
| 上述交易动作即时成交或部分成交 | `orderId`、`fillIds`、`accountEventIds`、`snapshot`；涉及仓位时返回 `positionId` | `fillIds` 至少包含本次新增成交；部分成交仍保留同一 `orderId` |
| `cancel_order` 成功 | `orderId`、`accountEventIds`、`snapshot` | 撤销已完成订单返回 `order_not_pending`，不得伪造成功 |
| `adjust_protection` 成功 | `positionId`、`accountEventIds`、`snapshot` | 不创建 `orderId` / `fillIds` |
| `record_invalidation_signal` 成功 | `positionId`、`accountEventIds`、`snapshot`；若生成 trigger 则返回 `triggerId` | 不自动创建订单或成交 |
| 参数预校验拒绝且无账户事实 | `reason`、`snapshot`、`accountEventIds = []` | 不返回对象 ID |
| 写入拒单事实后的拒绝 | `reason`、`orderId`、`rejectionEventId`、`accountEventIds`、`snapshot` | `rejectionEventId` 必须属于 `accountEventIds` |

#### `update_watchlist`

自选维护是非交易能力，可由前端用户、Agent tool 或系统维护流程调用。它不能创建订单、修改仓位或调整保护条件。

```ts
type UpdateWatchlistInput =
  | {
      action: "add";
      tsCode: TsCode;
      note?: string;
      reason?: string;
    }
  | {
      action: "remove";
      tsCode: TsCode;
      reason?: string;
    }
  | {
      action: "update_note";
      tsCode: TsCode;
      note?: string;
      reason?: string;
    };

type UpdateWatchlistResponse = {
  accepted: boolean;
  reason?: ErrorCode;
  message?: string;
  item?: WatchlistItem;
  accountEventIds: string[];
  warnings?: WarningCode[];
};
```

规则：

- `update_watchlist` 允许 `actor = user | agent | system`。
- `add` 必须校验 `tsCode` 是 Quotes 已知标的；未知标的返回 `not_found`。
- 重复 `add` 是幂等更新：不创建重复自选项，可以更新 `note`，并返回现有 item。
- `remove` 对不存在的自选项返回 `accepted = true` 且不创建重复删除事件，保证幂等。
- `update_note` 只修改自选备注，不影响订单、仓位、保护条件或 subscribed codes 之外的账户事实。
- 成功产生变化时必须写 `watchlist_added` / `watchlist_removed` / `watchlist_note_updated`；`accountEventIds` 顺序等于事件 append 顺序。

#### `account_reset`

重开一局模拟盘。用户主动操作（前端带确认弹窗）。语义见 §2「账户重置」。

```ts
type AccountResetResponse = {
  season: number;        // 新局序号（旧局归档时占用 season，新局 = season + 1 起算）
  snapshot: AccountSnapshot;  // 新局空局快照（cash = totalEquity = initial_cash）
};
```

规则：

- 单事务内：归档旧局摘要到 `account_archive` → 清空账户财务 live 表（保留 `account_watchlist`）→ 重新初始化账户（现金 = `initial_cash`，写 `account_initialized`）。
- 不触碰自选；不触碰 Agent 决策链（跨 BC，物理隔离）。
- 幂等性：重置不是幂等操作（每次都是新的一局），不接受 `clientOrderId`；前端确认弹窗负责防误触。

#### `list_account_archives`

列出历史归档局摘要（复盘取回）。

```ts
type AccountArchive = {
  season: number;
  resetAt: OccurredAt;
  initialCash: Money;
  finalCash: Money;
  finalEquity: Money;
  realizedPnl: Money;
  fillCount: number;
  closedPositionCount: number;
  eventCount: number;
};
type ListAccountArchivesResponse = { archives: AccountArchive[] };  // season desc
```

### 内部 Rust API

内部 API 以 command / query facade 为主：

```rust
initialize_account_if_needed(initial_cash) -> AccountSnapshot;
fetch_account(request) -> FetchAccountResponse;
operate_account(request, actor: "agent") -> OperateAccountResponse;
update_watchlist(request, actor: AccountActor) -> UpdateWatchlistResponse;
evaluate_account_triggers({ now, limit, cursor }) -> AccountTriggerResult;
mark_trigger_handled(trigger_id, reason) -> MarkTriggerHandledResponse;
rebuild_account_snapshot() -> AccountSnapshot;
reset_account() -> AccountResetResponse;
list_account_archives() -> ListAccountArchivesResponse;
subscribed_codes() -> Vec<TsCode>;
```

---

## 5. 模块独有功能

### 交易规则

| 规则 | 说明 |
|---|---|
| 可交易范围 | 只支持股票和场内基金；指数、未知标的、退市标的不能下单 |
| 整手 | 股票和场内基金买入 / 卖出数量必须是 100 股 / 份整数倍；本模拟统一简化为 100 股 / 份，不区分科创板 / 创业板 200 股门槛或奇数股卖出规则 |
| T+1 | 当日买入的 lot 当日不可卖 |
| 交易时段 | 即时成交类订单只在 A 股交易时段成交；挂单可盘外创建，交易时段再判断 |
| 行情新鲜度 | `market` 和即时成交类便捷动作必须使用 fresh quote；stale / missing quote 拒单 |
| 可成交性 | 买入需要卖盘可成交，卖出需要买盘可成交；盘口缺失时返回 `depth_missing` |
| 费用 | 佣金双向收取，印花税仅卖出收取 |
| 现金 | 买入不能超过可用现金；挂买单冻结现金 |
| 持仓 | 卖出不能超过可卖数量；挂卖单冻结对应可卖数量 |
| 涨跌停 / 停牌 | 停牌不得成交；涨停不可买入成交，跌停不可卖出成交，除非盘口证明可成交 |
| 硬风控 | 买入必须满足 `AccountRiskPolicy` |

Error code 规则：

| 条件 | `OperateAccountResponse.reason` |
|---|---|
| action 参数组合非法、数量小于等于 0、position 状态不允许该动作 | `invalid_input` |
| 标的不是股票 / 场内基金、退市或未知不可交易标的 | `instrument_not_tradable` |
| 即时成交读取到 `tradeStatus = "halted"` 或明确停牌 | `instrument_suspended` |
| 即时成交在非交易时段或 `tradeStatus = "closed"` | `outside_trading_session` |
| fresh quote 缺失 / stale / 关键价格缺失 | `quote_missing` / `quote_stale` / `quote_price_missing` |
| 需要盘口成交但买一 / 卖一不可用 | `depth_missing` |
| 涨停买入或跌停卖出且盘口不能证明可成交 | `limit_up_down_blocked` |
| 可用现金不足或风险预算不足 | `insufficient_cash` / `risk_limit_exceeded` |
| 可卖数量不足或 T+1 / 冻结导致不可卖 | `insufficient_sellable_quantity` |
| 数量不满足最小手数 | `invalid_lot_size` |
| 撤单目标不存在或不是 pending / partially_filled | `order_not_pending` |

### 订单成交模拟

- `market` 订单用 fresh Quotes snapshot 的当前价和盘口模拟成交。
- 买入成交价格优先使用一档卖价；卖出成交价格优先使用一档买价；缺盘口时不得成交。
- `limit` 买单在 fresh quote 的一档卖价 `ask[0].price <= limitPrice` 且卖盘可成交时成交；不得用 `quote.price` 替代可执行卖价。
- `limit` 卖单在 fresh quote 的一档买价 `bid[0].price >= limitPrice` 且买盘可成交时成交；不得用 `quote.price` 替代可执行买价。
- stale / missing quote 不得触发成交；pending 订单保持 pending 并等待下一次 fresh quote。
- `tradeStatus = "halted"` 时即时成交类动作必须返回 `instrument_suspended`；`tradeStatus = "closed"` 或非交易时段即时成交必须返回 `outside_trading_session`。
- 买入遇到涨停且卖盘不可成交、卖出遇到跌停且买盘不可成交时，新提交的即时成交类命令必须返回 `accepted = false` / `limit_up_down_blocked`；既有 pending limit 订单评估时保持 pending，不写成交事件。若 `limitUp` / `limitDown` 缺失导致无法判断，返回或记录 `quote_price_missing`。
- 盘口量不足时允许部分成交，但市价单和限价单的剩余量语义不同：`market` 订单部分成交后剩余数量立即自动取消，`partially_filled` 是终态，并同步写 `order_cancelled` 表达剩余取消；`limit` 订单部分成交后剩余数量继续保持 pending，可继续撮合 / 撤单 / 过期。部分成交只写 `order_partially_filled` / `position_scaled` 等账户事件并 emit `account-updated`，不创建 `AccountTrigger`，也不 emit `account-triggered`。
- 过期订单变为 `expired`，并释放冻结现金 / 冻结持仓。
- 评估大量订单 / 仓位时必须分批处理；每 tick 最多处理 `account_trigger_eval_batch_size` 条，结果返回 `has_more` / `next_cursor` 供下次继续。
- 批处理顺序必须稳定：先 pending orders，再 open positions；同一类按 `updated_at asc` / `created_at asc` 排序。
- `trigger_id` 生成规则必须使用“触发事件模型”中定义的统一稳定键，重试或分页不能产生重复触发。

### 保护条件触发

| 条件 | 多头触发规则 | 结果 |
|---|---|---|
| 止损 | `price <= stopLoss` | `account-triggered(stop_loss)` |
| 止盈 | `price >= takeProfit` | `account-triggered(take_profit)` |
| 时间止损 | `now >= timeStopAt` | `account-triggered(time_stop)` |
| 失效条件 | 外部信号命中 | `account-triggered(invalidated)` |

触发后 Account 只记录和通知。下游决策方可选择平仓、减仓、继续持有、调整保护条件或撤销挂单。

行情边界：

- 止损 / 止盈评估读取 Quotes snapshot；fresh quote 命中时正常生成 trigger。
- stale quote 命中价格型保护条件时仍可以生成 trigger，因为 trigger 只是通知不是成交；`AccountTrigger` 和 `AccountTriggeredPayload` 必须携带 `quoteFreshness`，并在 `warnings` 中包含 `quote_stale`。
- missing quote 或关键价格缺失时跳过价格型保护评估，不生成 trigger；本批次 `AccountTriggerResult.warnings` 必须包含 `quote_missing` 或 `quote_price_missing`，并记录可观测日志。
- `time_stop` 和 `invalidated` 不依赖行情 freshness。

调度期望：

- Account 不拥有 scheduler；`evaluate_account_triggers` 由 Agent Runtime / 应用调度层调用。
- **评估的数据依赖是有界的**：估值 / 保护条件评估 / 限价撮合只需要 `持仓 ∪ 挂单（∪ 自选）= subscribed_codes` 的行情，与全市场无关。因此调度层应在对 `subscribed_codes ∪ core_indexes` 做 **focused refresh**（同步、亚秒级）后立即触发评估，**而非搭全市场 universe 刷新的便车**（universe 后台 fallback 会引入几十秒延时）；并以固定 cadence 兜底。具体由 Agent Runtime 自驱 quote tick 编排（见 [agent-runtime-module.md](agent-runtime-module.md) §6），间隔和 batch size 以其 runtime settings 为准。
- 每次调用必须尊重 `limit` / `cursor`，若返回 `has_more = true`，调度层应继续分页直到本轮耗尽或达到调度预算。

### 订阅集合

Account 对外暴露当前关注集合：

```text
subscribed_codes = watchlist ∪ open_positions ∪ pending_orders
```

规则：

- `subscribed_codes()` 只是 Account 暴露给编排层的关注集合，不是行情刷新命令。
- Account 内部需要行情时，只读取 Quotes 已有 snapshot / query facade，用于估值、成交模拟和保护条件评估；标准读取路径是 `fetch_data({ tsCodes, include: { quote: true } })` 或同等内部 query facade。Account 不调用 Quotes provider，也不主动触发 refresh。
- Quotes 拥有 `core_indexes()` 和 `refresh_market_quotes({ scope: { kind: "subscribed", tsCodes }, purpose })`；Agent Runtime 负责调用 `Account.subscribed_codes()`、合并 `Quotes.core_indexes()`，再调用 Quotes refresh。
- 该跨模块调用流程以 [agent-runtime-module.md](agent-runtime-module.md) 为准；Account spec 只定义自己暴露的集合和读取 Quotes snapshot 的边界。

---

## 6. 验收标准 / 例子

- Account 读取入口为 `fetch_account`；人工 UI 没有交易写 command。
- Account 交易写入口为 `operate_account`。
- `operate_account` 只对 Agent tool / 外部自动化决策运行时暴露，人工 UI 和 `system` 维护流程都不能直接调用它创建订单。
- 自选维护入口为 `update_watchlist`；人工 UI 可以添加 / 删除自选或更新备注，但不能通过它创建订单、调整仓位或修改保护条件。
- `operate_account(open_position)` 会创建订单；成交后生成 fill、position、account event，并刷新 snapshot。
- `operate_account(open_position)` 使用 `limit` 时不能同时设置保护条件；限价开仓成交后由下游决策方再调用 `adjust_protection`。
- `operate_account(adjust_protection)` 只改保护条件，不自动创建卖单。
- `PositionProtection.stopLoss/takeProfit` 命中时只生成 trigger，不进入 `Order`，不冻结持仓，不自动成交。
- `operate_account(open_position)` / `market` 订单在 quote stale 或缺失时必须拒单，不能依赖调用方自律。
- 价格触及止损 / 止盈时，Account 写 `AccountTrigger` 并 emit `account-triggered`，不调用下游决策模块、不自动平仓。
- `fetch_account({ include: { watchlist: true } })` 返回自选列表和 Quotes snapshot 行情摘要；缺行情时返回 warning。
- `fetch_account({ include: { positions: true, snapshot: true } })` 返回仓位列表和账户总览；成本、市值、盈亏和总资产由 Account 派生。
- `AccountSnapshot` 的 cash / PnL / totalAssets 可由 events + fills + positions + Quotes snapshot 重算。
- `AccountSnapshot` 遇到部分仓位缺行情时必须返回 `data_partial`，并通过 `pricedPositionCount` / `unpricedPositionCount` 表达估值覆盖范围。
- 外部调度者可消费 `subscribed_codes()` 并注入行情刷新；Account 不直接刷新行情。
- 当日买入 lot 的 `sellableQuantity` 为 0；次一交易日才可卖。
- 挂买单冻结现金，撤单 / 过期释放冻结现金。
- 挂卖单冻结对应可卖数量，撤单 / 过期释放冻结数量。
- Account 任一层不 import 下游决策或资讯模块代码；只允许按架构规则读取 Quotes snapshot。

---

## 7. 模块边界外

这些能力不属于 Account 模块：

- 真券商交易。
- 投资观点生成。
- 新闻、研报、公告分析。
- 市场行情 provider。
- 下游决策 run 调度。
- 自动决定止损 / 止盈触发后的交易动作。
