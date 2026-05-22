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
- 费用参数、风控阈值默认值是 `Spec-anchored` 配置，但执行时必须 fail closed。

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
- 订单、仓位和保护条件写能力不通过人工 UI 暴露；只允许自动化 / 系统写入口。
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

type Order = {
  orderId: string;
  tsCode: TsCode;
  side: "buy" | "sell";
  orderType: "market" | "limit";
  limitPrice?: Price;
  quantity: Shares;
  filledQuantity: Shares;
  status: "pending" | "partially_filled" | "filled" | "cancelled" | "rejected" | "expired";
  intent:
    | "open_position"
    | "scale_in"
    | "scale_out"
    | "close_position"
    | "direct_order";
  positionId?: string;
  reason?: string;
  actor: TradingActor;
  createdAt: OccurredAt;
  updatedAt: OccurredAt;
  expiresAt?: OccurredAt;
};
```

Actor 命名规则：

- `agent` 表示由 Agent tool / 后台自动化决策流程发起的交易意图；Account 只记录 actor，不依赖 Agent 代码。
- `agent` 交易写动作必须经由 Agent tool / 后台自动化流程进入，不通过人工 UI 暴露。
- `system` 只用于账户内部维护任务，例如订单过期、挂单成交评估、snapshot 重建和初始化。
- `user` 只允许用于自选维护事件，不允许创建订单、调整仓位或调整保护条件。

字段说明：

| 字段 | 含义 | 规则 |
|---|---|---|
| `orderId` | 委托唯一 ID | 创建后不可变 |
| `tsCode` | 标准标的代码 | 必须是 Quotes 已知且 Account 可交易的 `TsCode` |
| `side` | 买入 / 卖出方向 | 买入冻结现金，卖出冻结可卖数量 |
| `orderType` | 市价 / 限价 | `market` immediate-or-reject；`limit` 可 pending |
| `limitPrice` | 限价价格 | `orderType = limit` 时必填 |
| `quantity` | 委托股 / 份数量 | 股票和场内基金必须 100 股 / 份整数倍 |
| `filledQuantity` | 已成交数量 | 由成交回报累加，不能超过 `quantity` |
| `status` | 订单状态 | 只能按订单状态机转换 |
| `intent` | 业务意图分类 | 用于审计和复盘，不替代 `side/orderType` |
| `positionId` | 关联仓位 | 加仓、减仓、平仓时必填；开仓成交后回填 |
| `reason` | 发起理由 | 写操作必须提供，用于审计 |
| `actor` | 发起者 | 订单只能由 `agent` 或 `system` 发起；`user` 不允许创建订单 |
| `createdAt` / `updatedAt` | 创建 / 更新时间 | ISO-8601 |
| `expiresAt` | 订单过期时间 | 过期后释放冻结现金 / 持仓 |

规则：

- `open_position` / `scale_position` / `close_position` 可以作为写入口的便捷动作，但 Account 内部仍应落为订单 + 成交 + 仓位事件。
- `market` 表示用当前 fresh Quotes snapshot 模拟即时成交；它是 immediate-or-reject，不允许进入 pending。
- `limit` 表示挂单；由定时任务根据 Quotes snapshot 判断是否成交、部分成交、过期。
- `limit` 订单可以在 quote stale 时创建为 pending，但不能在 stale quote 上成交。
- Account 交易只支持 `InstrumentCategory = "stock" | "fund"`；`index` 只能用于行情展示和市场背景，不能下单。
- 股票和场内基金买入 / 卖出数量必须是 100 股 / 份整数倍。
- 标的不存在返回 `not_found`；标的不是 `stock` / `fund` 返回 `instrument_not_tradable`；标的停牌或退市状态不可成交，使用 `instrument_suspended` 或 `instrument_not_tradable`。
- `Order.intent` 必须由写入口确定：
  - `place_order` -> `direct_order`，表示通用委托意图；`actor` 仍只能是 `agent` / `system`，不表示人工 UI 交易。
  - `open_position` -> `open_position`。
  - `scale_position(side = "increase")` -> `scale_in`。
  - `scale_position(side = "decrease")` -> `scale_out`。
  - `close_position` -> `close_position`。
- `direct_order` 是低阶委托语义：买入成交时若无 open position 则生成 `position_opened`，若已有 open position 则生成 `position_scaled`；卖出成交后若剩余持仓大于 0 则生成 `position_scaled`，若剩余为 0 则生成 `position_closed`。事件 payload 必须保留来源订单的 `intent = "direct_order"`。

订单创建评估和状态机：

| 当前阶段 / 持久状态 | 允许转换到 | 触发 |
|---|---|---|
| `creating` | `filled` | `market` 订单即时成交 |
| `creating` | `rejected` | 参数、交易时段、行情、现金、持仓、风控等校验失败且订单已创建 |
| `creating` | `pending` | `limit` 订单创建并冻结资金 / 持仓成功 |
| `pending` | `partially_filled` | 挂单部分成交 |
| `pending` | `filled` | 挂单全部成交 |
| `pending` | `cancelled` | 显式撤单 |
| `pending` | `expired` | 到期未完全成交 |
| `partially_filled` | `filled` | 剩余数量继续成交完成 |
| `partially_filled` | `cancelled` | 显式撤销剩余数量 |
| `partially_filled` | `expired` | 剩余数量到期 |

规则：

- `creating` 是创建命令内部的评估阶段，不是 `Order.status`，不得持久化或对外返回。
- `filled`、`cancelled`、`rejected`、`expired` 是终态，不能再转换。
- `market` 订单只能从 `creating` 到 `filled` 或 `rejected`，不得持久化为 `pending`。
- `limit` 订单进入 `pending` 前必须完成冻结；冻结失败则拒绝，不得留下可成交挂单。
- `partially_filled` 的 `filledQuantity` 必须大于 0 且小于 `quantity`。

### 成交模型

```ts
type TradeFill = {
  fillId: string;
  orderId: string;
  positionId?: string;
  tsCode: TsCode;
  side: "buy" | "sell";
  price: Price;
  quantity: Shares;
  commission: Money;
  stampTax: Money;
  occurredAt: OccurredAt;
};
```

字段说明：

| 字段 | 含义 | 规则 |
|---|---|---|
| `fillId` | 成交唯一 ID | append-only，不可变 |
| `orderId` | 来源订单 ID | 必须指向已接受订单 |
| `positionId` | 影响的仓位 ID | 开仓首笔成交可生成新仓位 |
| `tsCode` | 标的代码 | 与订单一致 |
| `side` | 成交方向 | 与订单方向一致 |
| `price` | 成交价 | 买入优先卖一价，卖出优先买一价 |
| `quantity` | 成交数量 | 不得超过订单剩余数量 |
| `commission` | 佣金 | 双向收取 |
| `stampTax` | 印花税 | 仅卖出收取 |
| `occurredAt` | 成交时间 | 必须在可成交交易时段内 |

规则：

- 买入成交减少现金，卖出成交增加现金。
- 佣金双向收取，印花税仅卖出收取。
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
- `Position.sellableQuantity = sum(max(remainingQuantity - frozenQuantity, 0))`，仅统计 `sellableFrom <= currentTradeDate` 的 lot。
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
  actor: TradingActor;
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
| `actor` | 初始开仓发起者 | `agent` 或 `system`，用于审计 |
| `reasoning` | 开仓理由摘要 | 来自外部决策方 thesis 或系统说明 |
| `warnings` | 仓位估值警告 | 行情缺失 / stale / 部分估值等 |

规则：

- `avgCost` 由成交记录加权派生。
- `sellableQuantity` 由 `PositionLot` 和 T+1 规则派生。
- `marketPrice`、`marketValue`、`unrealizedPnl` 来自 Quotes snapshot 派生。
- 同一 `ts_code` 默认只有一个 open position；新增买入成交若已有 open position，读模型合并为加仓。
- 高阶 `open_position(tsCode)` 如果该标的已有 open position，必须拒绝并返回 `invalid_input`；下游决策方需要显式调用 `scale_position(side = "increase")`，避免新的开仓理由被隐式挂到既有仓位上。
- `Position.actor` 固定表示初始开仓发起者；后续调仓、平仓、保护条件调整由 `AccountEvent.actor` 审计，不回写为“当前管理者”。
- 全部数量卖出后仓位关闭。

### 保护条件模型

```ts
type PositionProtection = {
  stopLoss?: Price;
  takeProfit?: Price;
  timeStopAt?: OccurredAt;
  invalidationSignals?: string[];
  enabled: boolean;
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
| `updatedAt` | 最近更新时间 | 调整保护条件时刷新 |

规则：

- 对多头仓位：`stopLoss < currentPrice`，`takeProfit > currentPrice`。
- 条件命中只生成 `AccountTrigger` 和 `AccountEvent`，不默认自动平仓。
- 下游决策方可在收到触发后调用 `operate_account` 平仓、减仓、撤单或调整保护条件。
- 同一保护条件连续命中时必须去重，避免同一价格 tick 反复触发下游响应。
- `invalidationSignals` 是可选的“持仓理由失效标签”集合。例如开仓理由依赖 `业绩修复`，下游决策方可以设置 `["earnings_recovery_failed"]`；当外部明确调用 `record_invalidation_signal(signal = "earnings_recovery_failed")` 时，Account 做精确匹配并生成 `invalidated` trigger。
- Account 不解析新闻、策略或自然语言，不判断信号是否成立；它只保存标签、记录外部显式 signal，并做字符串精确匹配。
- `adjust_protection.invalidationSignals` 采用全量替换语义：字段缺省表示不修改，传空数组表示清空；不得隐式 merge，避免旧失效条件残留。
- 同一 open position 的同一 `signal` 在仓位生命周期内只生成一次未处理 `invalidated` trigger；需要区分不同事实时，下游决策方应使用不同 signal 标签。
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
| `valuationFreshness` | 账户估值新鲜度 | 由参与估值的 Quotes snapshot 派生 |
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
- `unrealizedPnl = marketValue - remainingCostBasis`，仅在 fresh 或可展示的 stale quote 存在时计算；响应必须携带 quote freshness。
- `realizedPnl` 由卖出成交收入减去被卖出 lot 的成本、佣金和印花税派生。
- `AccountSnapshot.marketValue` 只汇总成功估值的 open positions；缺行情或不可用行情的仓位不按 0 伪造估值，必须计入 `unpricedPositionCount`。
- `AccountSnapshot.totalAssets = cash + marketValue`；当 `unpricedPositionCount > 0` 时这是部分估值结果，`warnings` 必须包含 `data_partial`，不能被解释为完整账户净值。
- `valuationFreshness.status` 反映本次估值使用行情的最弱 freshness：全部可估值且 fresh 为 `fresh`，使用 stale 展示行情为 `stale`，无任何 open position 可估值或全部缺失时为 `missing`。
- `cash`、`frozenCash`、`availableCash`、`totalAssets` 不得由前端或 Agent 自行计算后写回。
- Account 读取接口可以返回 stale quote 参与展示估值，但交易写路径必须 fail closed，不能用 stale / missing quote 成交。

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
- `payload` 保存足以重放读模型的最小事实，不保存 UI 展示冗余。
- 拒单若订单已经创建，必须写 `order_rejected`；参数校验在订单创建前失败时可以只返回 rejection response。
- 创建 `AccountTrigger` 时必须有可追溯的 `AccountEvent`。订单成交 / 拒绝 / 过期触发可复用对应订单事件作为 `AccountTrigger.eventId`；保护条件和失效信号触发必须写 `trigger_created` 事件并使用其 `eventId`。

### 冻结和重建规则

冻结现金 / 持仓是 pending 订单的派生约束，不是独立真源。`cash_frozen`、`cash_released`、`shares_frozen`、`shares_released` 是审计事件；重建时必须能由订单、成交、lot 和这些事件校验一致性。

规则：

- `limit` 买单进入 `pending` 前必须冻结预计最大占用现金：`limitPrice * remainingQuantity + estimatedFees`。
- `market` 买单即时成交，不保留长期冻结；若成交前需要内部冻结，必须在同一写事务内释放或扣减。
- 买单部分成交时，成交部分转为实际现金扣减；未成交部分继续冻结，若实际成交价低于冻结价，差额必须释放。
- 买单撤单 / 过期时，释放该订单剩余未成交数量对应的冻结现金。
- `limit` 卖单进入 `pending` 前必须从可卖 lot 中冻结对应数量；冻结失败返回 `insufficient_sellable_quantity`。
- 卖单部分成交时，成交部分从 frozen lot 转为 sold；未成交部分继续冻结。
- 卖单撤单 / 过期时，释放该订单剩余未成交数量对应的 frozen lot。
- `AccountSnapshot.frozenCash` 由未完成买单剩余冻结金额派生；`Position.sellableQuantity` 必须扣除 frozen lot。
- 重建读模型时，若冻结事件和订单 / lot 派生结果不一致，必须 fail closed，返回 `duplicate_event` 或 `db_error`，并记录可观测日志；不能静默修正现金或持仓。

### 触发事件模型

```ts
type AccountTriggerType =
  | "stop_loss"
  | "take_profit"
  | "time_stop"
  | "order_filled"
  | "order_rejected"
  | "order_expired"
  | "invalidated";

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
      ok: true;
      trigger: AccountTrigger;
      accountEventIds: string[];
    }
  | {
      ok: false;
      reason: ErrorCode;
      message?: string;
    };
```

字段说明：

| 字段 | 含义 | 规则 |
|---|---|---|
| `triggerId` | 跨模块触发幂等 ID | Orchestrator / 下游决策方用它去重 |
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
- `handled` 表示 Account 已确认该 trigger 被下游消费或显式忽略；不表示下游决策方采取了交易动作。
- 同一保护条件按下方 `triggerId` 确定性键只能生成一个 trigger。
- `triggerId` 必须按稳定字段确定性生成：
  - 价格型持仓保护：`triggerType + positionId + tsCode + threshold + tradeDate`。
  - 时间止损：`triggerType + positionId + tsCode + timeStopAt`。
  - 失效信号：`triggerType + positionId + tsCode + signal`。
  - 订单终态：`triggerType + orderId + tsCode + 对应终态 AccountEvent.eventId`。
  盘中连续 tick、分页重试和进程重启不得重复生成相同 trigger。
- `mark_trigger_handled(trigger_id, reason)` 是内部维护 API，用于 Orchestrator 成功路由、显式忽略或恢复处理后确认 trigger 不再需要重复路由。
- `mark_trigger_handled` 必须幂等：已 handled 的 trigger 再次标记仍返回同一 trigger，不重复写事件。
- 首次标记 handled 必须写 `trigger_handled` 事件，并将 `AccountTrigger.handled` 置为 true；未知 `trigger_id` 返回 `ok = false` / `not_found`。

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
- `hasMore = true` 时必须返回 `nextCursor`，Orchestrator 必须使用它继续调度；Account 不在单个 tick 内无限循环。
- `nextCursor` 必须基于持久排序键生成，例如 `phase + updatedAt/createdAt + orderId/positionId`，不得使用内存 offset；它必须可跨进程重启后恢复同一批次之后的扫描位置。

### 硬风控模型

```ts
type AccountRiskPolicy = {
  maxSinglePositionRatio: Ratio;
  maxGrossExposureRatio: Ratio;
  maxOrderValueRatio: Ratio;
  maxDailyNewOrders: number;
};
```

默认规则：

- 自动化交易写动作必须携带可审计 reason；是否存在 active strategy 由下游决策纪律保证，不属于 Account 依赖。
- 任何买入后单票市值超过 `maxSinglePositionRatio` 必须拒绝。
- 任何买入后总仓位超过 `maxGrossExposureRatio` 必须拒绝。
- 单笔订单金额超过 `maxOrderValueRatio * totalAssets` 必须拒绝。
- `maxDailyNewOrders` 按 `Asia/Shanghai` 自然日统计 `actor = agent` 新创建订单数；撤单 / 过期不扣减，跨日遗留 pending 订单不计入新一天，`system` 维护动作不计入。
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
  -> upsert orders / fills / positions / lots or watchlist
  -> rebuild account snapshot
  -> emit account-updated
```

规则：

- 所有写操作串行化，避免现金、仓位、订单并发漂移。
- 交易写操作必须有 `actor = agent | system` 和可审计 note/reason。
- 失败的写操作也应能返回明确 rejection reason；是否写 rejection event 由订单是否已创建决定。
- `account-updated` payload 使用 [shared-types.md](shared-types.md) 定义的 `AccountUpdatedPayload`，至少包含本次 append 的 `accountEventIds` 和重建后的 `snapshotCapturedAt`；消费者收到后应按需重新读 `fetch_account`。

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
  orderStatus?: "open" | "pending" | "partially_filled" | "filled" | "cancelled" | "rejected" | "expired" | "all";
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

- `positionStatus` 只过滤 `positions`，默认 `open`；`orderStatus` 只过滤 `orders`，默认 `open`，表示 `pending + partially_filled`。两者互不影响，`all` 返回对应集合全部状态；不影响 `snapshot` / `watchlist`。
- `limit` 默认 100，最大 500；`offset` 默认 0。分页应用到 `positions`、`orders`、`events`、`triggers` 这些可增长集合；`snapshot` 和 `watchlist` 不分页。
- 人工 UI 不能通过 Tauri command 做交易写操作。
- `operate_account` 只允许 Agent tool / 自动化运行时 / system maintenance 调用；前端不能绕过 Agent 下单。
- 前端可以通过非交易写入口 `update_watchlist` 添加 / 删除自选或更新备注。
- 展示自选时，Account 返回自选元信息，行情字段来自 Quotes snapshot。
- `fetch_account` 是统一读取 facade；前端或 Agent 可以通过 `include.watchlist`、`include.positions`、`include.snapshot` 获取自选、仓位和账户总览。实现可以提供轻量 wrapper，但不得绕过同一套 Account query 规则。

### 写入接口

#### `operate_account`

```ts
type OperateAccountInput =
  | {
      action: "place_order";
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
      orderId: string;
      reason: string;
    }
  | {
      action: "open_position";
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

- 所有 action 都必须写入可审计事件。
- `open_position`、`scale_position`、`close_position` 是便捷交易动作，内部仍走订单 / 成交流。
- `open_position` 必须只用于建立新标的仓位；如果 `tsCode` 已存在 open position，必须返回 `accepted = false` / `invalid_input`。加仓必须显式使用 `scale_position(side = "increase")`。
- `place_order` 是低阶委托入口，不做开仓 / 加仓 / 平仓意图推断；成交后按持仓是否存在和剩余数量派生 `position_opened` / `position_scaled` / `position_closed` 事件。
- `scale_position.side = "increase"` 内部生成买入 / 加仓订单，`side = "decrease"` 内部生成卖出 / 减仓订单；`quantity` 必须为正数，不能用负数表达方向。
- `scale_position.side = "decrease"` 默认只做部分减仓；若 `quantity` 等于全部持仓，应使用 `close_position`，避免复盘语义混淆。
- `adjust_protection` 只调整保护条件，不直接下单。
- `adjust_protection` 字段缺省表示“不修改该字段”；`stopLoss` / `takeProfit` / `timeStopAt` 传 `null` 表示清除该条件，传具体值表示设置或替换；`enabled` 缺省表示不修改，传 `true` / `false` 表示启用 / 禁用整组保护条件。
- `open_position` / `scale_position` / `close_position` 的 `orderType` 缺省时必须按 `"market"` 处理；`place_order` 必须显式传 `orderType`。
- `open_position` / `scale_position` / `close_position` 使用 `limit` 时可以设置 `expiresAt`；未设置时按当日有效委托处理，非交易时段创建则默认到下一交易日收盘过期。
- `open_position` 使用 `limit` 且可能进入 pending 时，不允许同时携带 `stopLoss` / `takeProfit` / `timeStopAt`，避免给未存在的仓位设置保护条件；成交后由下游决策方通过 `adjust_protection` 设置。
- `record_invalidation_signal` 只记录外部显式信号；调用时必须先写 `invalidation_signal_recorded` 事件。当 `signal` 精确命中该仓位启用中的 `invalidationSignals` 时，再生成 `invalidated` trigger，不自动交易。
- 返回必须包含 `accepted/rejected`、订单或仓位 ID、错误原因、最新 snapshot 摘要。
- 交易 action 必须先校验标的可交易性；非股票 / 场内基金返回 `instrument_not_tradable`。
- 即时成交类 action 遇到 stale / missing quote 必须返回 `rejected`，reason 使用 `quote_stale` / `quote_missing` / `quote_price_missing`。

响应契约：

```ts
type OperateAccountResponse = {
  accepted: boolean;
  reason?: ErrorCode;
  message?: string;
  orderId?: string;
  fillIds?: string[];
  positionId?: string;
  triggerId?: string;
  accountEventIds: string[];
  snapshot: AccountSnapshot;
  warnings?: WarningCode[];
};
```

规则：

- `accepted = true` 只表示 Account 接受并处理了命令；`limit` 订单可能仍是 pending。
- `market` 订单 accepted 时必须已经成交或明确创建了 rejection event；不能返回 pending。
- rejected 响应必须有 `reason`。
- 有副作用的 rejected 操作必须返回对应 `accountEventIds`。
- `accountEventIds` 的顺序必须等于事件 append 顺序，供审计链展示。

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

### 内部 Rust API

内部 API 以 command / query facade 为主：

```rust
fetch_account(request) -> FetchAccountResponse;
operate_account(request, actor: TradingActor) -> OperateAccountResponse;
update_watchlist(request, actor: AccountActor) -> UpdateWatchlistResponse;
evaluate_account_triggers({ now, limit, cursor }) -> AccountTriggerResult;
mark_trigger_handled(trigger_id, reason) -> MarkTriggerHandledResponse;
rebuild_account_snapshot() -> AccountSnapshot;
subscribed_codes() -> Vec<TsCode>;
```

---

## 5. 模块独有功能

### 交易规则

| 规则 | 说明 |
|---|---|
| 可交易范围 | 只支持股票和场内基金；指数、未知标的、退市标的不能下单 |
| 整手 | 股票和场内基金买入 / 卖出数量必须是 100 股 / 份整数倍 |
| T+1 | 当日买入的 lot 当日不可卖 |
| 交易时段 | 即时成交类订单只在 A 股交易时段成交；挂单可盘外创建，交易时段再判断 |
| 行情新鲜度 | `market` 和即时成交类便捷动作必须使用 fresh quote；stale / missing quote 拒单 |
| 可成交性 | 买入需要卖盘可成交，卖出需要买盘可成交；盘口缺失时返回 `depth_missing` |
| 费用 | 佣金双向收取，印花税仅卖出收取 |
| 现金 | 买入不能超过可用现金；挂买单冻结现金 |
| 持仓 | 卖出不能超过可卖数量；挂卖单冻结对应可卖数量 |
| 涨跌停 / 停牌 | 停牌不得成交；涨停不可买入成交，跌停不可卖出成交，除非盘口证明可成交 |
| 硬风控 | 买入必须满足 `AccountRiskPolicy` |

### 订单成交模拟

- `market` 订单用 fresh Quotes snapshot 的当前价和盘口模拟成交。
- 买入成交价格优先使用一档卖价；卖出成交价格优先使用一档买价；缺盘口时不得成交。
- `limit` 买单在 fresh quote 满足 `quote.price <= limitPrice` 且卖盘可成交时成交。
- `limit` 卖单在 fresh quote 满足 `quote.price >= limitPrice` 且买盘可成交时成交。
- stale / missing quote 不得触发成交；pending 订单保持 pending 并等待下一次 fresh quote。
- 盘口量不足时允许部分成交，剩余数量保持 pending；部分成交只写 `order_partially_filled` / `position_scaled` 等账户事件并 emit `account-updated`，不创建 `AccountTrigger`，也不 emit `account-triggered`。
- 过期订单变为 `expired`，并释放冻结现金 / 冻结持仓。
- 评估大量订单 / 仓位时必须分批处理；每 tick 最多处理 `account.trigger_eval_batch_size` 条，结果返回 `has_more` / `next_cursor` 供下次继续。
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

### 订阅集合

Account 对外暴露当前关注集合：

```text
subscribed_codes = watchlist ∪ open_positions ∪ pending_orders
```

规则：

- `subscribed_codes()` 只是 Account 暴露给编排层的关注集合，不是行情刷新命令。
- Account 内部需要行情时，只读取 Quotes 已有 snapshot / query facade，用于估值、成交模拟和保护条件评估；Account 不调用 Quotes provider，也不主动触发 refresh。
- Quotes 拥有 `core_indexes()` 和 `refresh_market_quotes(scope)`；Orchestration 负责调用 `Account.subscribed_codes()`、合并 `Quotes.core_indexes()`，再调用 Quotes refresh。
- 该跨模块调用流程以 [orchestration.md](orchestration.md) 为准；Account spec 只定义自己暴露的集合和读取 Quotes snapshot 的边界。

---

## 6. 验收标准 / 例子

- Account 读取入口为 `fetch_account`；人工 UI 没有交易写 command。
- Account 交易写入口为 `operate_account`。
- `operate_account` 只对 Agent tool / 自动化运行时 / system maintenance 暴露，人工 UI 不能直接调用它创建订单。
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
