# Spec Guidelines

> 本文档定义本项目如何写 spec。所有实现、计划、测试和重构都以最新 spec 为准。

## 一句话定位

Spec 是**可执行设计契约**，不是实现流水账，也不是愿望清单。

它必须让实现者能回答：

```text
这个模块拥有什么领域概念？
哪些数据是契约，哪些只是实现细节？
哪些状态和状态转换合法？
哪些输入必须拒绝？
哪些事件必须产生？
如何证明实现符合契约？
```

---

## 1. Spec 分层

### Spec-as-source

这些内容必须和代码完全一致，应该能被测试或生成校验：

- 对外 command / query / event 的输入输出 DTO。
- 领域实体、值对象、状态枚举、错误码、warning code。
- 状态机和不变量。
- 幂等键、关联 ID、事件 payload。
- 持久化真源和必须可重建的读模型。
- 验收标准。

### Spec-anchored

这些内容约束实现方向，但允许内部实现随技术选择变化：

- 数据流和调用顺序。
- provider fallback 策略。
- refresh / scheduler 的默认频率。
- cache / snapshot 的生命周期语义。
- 推荐代码位置。

### Guidance

这些内容是设计方向，不应作为字段级契约：

- UI 构图、视觉风格、交互模式。
- 后续扩展范围。
- 非关键性能优化建议。

规则：

- 模块 spec 中未标注的领域模型、接口、事件和不变量默认视为 `Spec-as-source`。
- provider、cache、scheduler、UI 布局默认视为 `Spec-anchored` 或 `Guidance`，除非明确影响外部行为。

---

## 2. 数据模型写法

Spec 中应该写数据模型，但只写契约模型：

| 类型 | 是否进 spec | 写法 |
|---|---:|---|
| 领域模型 | 必须 | 类型、字段、状态、规则、不变量 |
| 对外 DTO | 必须 | 完整 request / response / event payload |
| 持久化真源 | 必须 | 真源对象、身份、唯一性、可重建关系 |
| 读模型 | 应该 | 查询语义、派生来源、stale / partial 规则 |
| SQL / 索引 / cache 结构 | 不写在模块 spec | 放到 migration 或 reference |
| 当前实现文件结构 | 不写 | 放到实现计划，不放 spec |

推荐 TypeScript-like 契约写法：

```ts
type ExampleCommand = {
  id: string;
  action: "create" | "cancel";
  reason: string;
};
```

领域模型字段必须跟随字段说明表，尤其是 ID、状态、时间、来源、派生字段和审计字段：

```text
| 字段 | 含义 | 规则 |
|---|---|---|
| id | 领域对象唯一 ID | 创建后不可变 |
| status | 当前状态 | 只能按状态机转换 |
```

规则：

- 字段说明不重复类型本身，而解释业务含义、来源、是否派生、是否可为空和约束。
- `source` 只用于数据来源 / provider；动作发起者使用 `actor` / `addedBy` / `createdBy`。
- 派生字段必须说明派生来源，不能让实现把它当真源。

模块 spec 不写 `create table`、索引、FTS 语法或具体 migration。需要约束持久化时，写成领域级契约：

```text
持久化契约：
- 真源：AccountEvent
- 身份：eventId 全局唯一
- 读模型：Order / TradeFill / Position / AccountSnapshot
- 派生：cash / PnL / totalAssets 可由事件和行情重建
- 禁止：snapshot 不能作为不可重建真源
```

SQLite schema、索引和 FTS 细节属于实现或 reference，不属于模块领域规范。

---

## 3. 模块 Spec 模板

每个 bounded context spec 推荐结构：

```text
定位
责任边界
领域模型
  - 领域词汇
  - 实体 / 值对象 / 聚合
  - 状态机
  - 不变量
数据流
对外接口
  - Commands
  - Queries
  - Events
内部 Rust API
模块独有功能
幂等 / 并发 / 失败语义
验收标准 / 例子
不纳入范围
```

写作规则：

- 只约束本 BC 自己拥有的数据、规则、接口和事件。
- 不替其他 BC 规定行为。
- 执行模块不定义 Agent 工具 schema；Agent 工具注册协议只写在 `agent-infra-module.md`。
- Agent 如何消费模块能力、每类 run 使用哪些工具、跨模块事件路由、定时调度和订阅注入只写在 `agent-runtime-module.md`。
- 共享类型只写在 `shared-types.md`，模块 spec 只引用。
- DTO 里出现标的、价格、数量、金额、时间时，优先使用共享类型名，如 `TsCode` / `Price` / `Shares` / `Money` / `OccurredAt`。
- provider / channel 细节写在 `docs/design/references/<bc>/`，模块 spec 只写选择策略和 canonical contract。

---

## 4. Provider / Channel Reference

Provider reference 是 adapter 级契约，路径固定：

```text
docs/design/references/quotes/<provider>.md
docs/design/references/news/<provider>.md
docs/design/references/agent/<wire-format>.md
```

模块 spec 写：

- 该类数据默认使用哪些 provider。
- 主源 / fallback 顺序。
- fallback 条件。
- provider 输出必须 normalize 到哪个 canonical model。
- provider 失败时模块对外如何返回 partial / warning / error。

Reference 写：

- 数据来源和获取方式。
- auth / token / base_url / server 选择。
- 请求参数、分页、batch size、timeout、retry、rate limit。
- 原始字段到 canonical model 的映射。
- 单位转换和代码 / 时间 normalize。
- 支持范围、已知缺口和 fallback 条件。
- fixture / adapter 验收标准。

规则：

- 模块 spec 不写厂商接口手册。
- Reference 不定义模块对外 API；它只约束 adapter 如何产出模块 canonical contract。
- 非官方或易变 endpoint 不作为模块 spec 的强契约；在 reference 中标为 adapter 细节。
- 新增 provider 必须新增 reference，再接入模块 provider 策略。

---

## 5. 契约用词

为了降低歧义，spec 中使用以下语义：

| 词 | 含义 |
|---|---|
| 必须 / must | 实现不满足就是 bug |
| 不允许 / must not | 实现出现就是 bug |
| 应该 / should | 默认要求；偏离必须有明确理由 |
| 可以 / may | 合法选择，不构成要求 |
| 默认 | 没有显式配置时的行为 |

避免使用：

- “尽量”
- “最好”
- “后续再说”
- “大概”

如果是第一阶段不做，写入“不纳入范围”或“第一阶段不要求”，但不要让核心契约留空。

---

## 6. 逻辑完整性

Spec 的逻辑描述必须完整到“实现者不需要猜”的程度。每个能力至少说明：

- 触发条件：谁在什么情况下调用。
- 输入：必填字段、可选字段、默认值、非法输入。
- 处理规则：状态如何变化、使用哪些本地事实、是否允许副作用。
- 输出：成功返回什么、partial success 怎么表达。
- 失败语义：哪些情况必须拒绝、warning / error code 是什么。
- 幂等：重复调用、重试、分页或并发时如何避免重复效果。
- 边界：哪些行为明确不属于本模块。

禁止只写抽象口号，例如：

```text
坏：刷新行情并处理异常。
好：盘中 refresh_market_quotes({ scope, purpose }) 对 scope 内 SH/SZ 先走 TDX；BJ 走 Eastmoney；
    单个 provider 失败只影响对应 item，返回 provider_partial_failure，不清空旧 snapshot。
```

规则：

- 如果存在 fallback，必须写 fallback 顺序和触发条件。
- 如果存在状态机，必须写合法状态和转换规则。
- 如果存在派生字段，必须写派生来源。
- 如果存在跨模块消费，必须写事件和幂等键。

---

## 7. 错误和 Partial 规则

所有对外接口必须说明：

- 整体失败条件。
- item 级失败条件。
- warning / error code。
- 缺数据时是否返回 partial result。
- stale 数据是否可用。
- 是否允许副作用。

默认规则：

- 批量读取接口优先 partial success，单个 item 缺数据不让整批失败。
- 写接口必须 fail closed，不能在不确定状态下假装成功。
- 交易相关写接口遇到 stale / missing quote 必须拒绝。
- 所有 rejection 必须返回稳定 reason code。

---

## 8. 幂等和审计

任何会触发后台 run、账户写入或跨模块消费的能力，spec 必须定义：

- 幂等键。
- 关联 ID。
- 重试后的行为。
- 是否持久化事件。
- 是否可重放。
- 审计链如何追踪。

默认规则：

- 账户状态变化先写 append-only event，再更新读模型。
- Agent 交易动作必须能从 `TradeIntent -> Account result -> DecisionEpisode` 追溯。
- Agent Runtime 负责跨模块事件消费幂等。

---

## 9. 验收标准写法

验收标准必须可检查。推荐格式：

```text
ACCT-ORD-001: market 买单遇到 stale quote 返回 rejected / quote_stale，不创建 fill。
ACCT-LOT-001: 当日买入 lot 的 sellableQuantity 为 0，下一交易日才可卖。
```

规则：

- 每条验收标准只检查一个行为。
- 验收标准应该能映射到测试名、lint 或人工检查步骤。
- 领域不变量必须至少有一个验收标准覆盖。
- 跨模块行为写到 `agent-runtime-module.md` 的验收标准，不塞进执行模块。

---

## 10. 实现符合 Spec 的方式

实现阶段应补以下护栏：

- Rust / TypeScript DTO 与 spec 同名，字段语义一致。
- 对外 command / query / event 做 contract tests。
- 持久化 / migration 做契约测试。
- Account / Agent 核心不变量做 domain tests。
- 分层依赖做 architecture lint。
- 每个 PR 标明修改了哪些 spec 和哪些验收标准。

如果实现必须偏离 spec，先改 spec，再改代码。
