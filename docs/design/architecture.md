# Architecture

> 本文档是整体设计入口。模块级领域契约以 `docs/design/*-module.md` 为准。
>
> 当前阶段只保留最新设计入口和跨模块约束；详细领域模型进入各模块 spec。

## 一句话定位

A 股研究 + 模拟交易学习终端。Agent 从市场数据和资讯中识别机会，在模拟账户中验证，并把过程沉淀成可审计、可复盘的判断链。

不连接真券商，只做模拟交易。

---

## 1. 模块 Specs

| Bounded Context | Spec | 职责 |
|---|---|---|
| Quotes | [quotes-module.md](quotes-module.md) | 市场数据本地读模型、行情、K 线、指标、基本面、扫描 |
| News | [news-module.md](news-module.md) | 多源资讯本地读模型、正文缓存、检索 |
| Account | [account-module.md](account-module.md) | 模拟券商账户、订单、成交、仓位、账户估值、自选列表 |
| Agent | [agent-module.md](agent-module.md)；[agent-infra-module.md](agent-infra-module.md)；[agent-runtime-module.md](agent-runtime-module.md) | Infra 负责模型渠道 / 消息 / 上下文 / 工具注册协议 / 基础 loop；Runtime 负责事件路由 / 调度 / 订阅注入 / 运行 mode（对话 / news / 复盘）/ 工具使用策略 / 决策审计 |

Spec 写作规范以 [spec-guidelines.md](spec-guidelines.md) 为准。

跨模块共享类型以 [shared-types.md](shared-types.md) 为准。

Provider / channel adapter 细节以 [references/](references/) 下的文档为准；模块 spec 只写 provider 选择策略和 canonical contract。

跨模块前端体验和视觉系统以 [../frontend-design.md](../frontend-design.md) 为准。

跨模块事件路由、后台任务和订阅集注入以 [agent-runtime-module.md](agent-runtime-module.md) 为准。

---

## 2. 分层约束

后端按 4 层组织，依赖方向单向：

```text
adapters
  -> pipeline
  -> infrastructure
  -> domain
```

| 层 | 职责 |
|---|---|
| `domain/` | 纯类型、规则、不变量、纯计算 |
| `infrastructure/` | SQLite、HTTP、provider、cache、snapshot |
| `pipeline/` | application use cases、后台任务、跨 infra 编排 |
| `adapters/` | Tauri commands、Agent tools、外部协议 DTO |

硬约束：

- `domain/` 不依赖 Tauri / SQLite / HTTP / infrastructure / pipeline / adapters。
- `infrastructure/` 不依赖 pipeline / adapters。
- `pipeline/` 不依赖 adapters。
- Quotes / Account / News 不 import Agent 代码。

---

## 3. 模块关系

```text
Agent Runtime
  -> News.refresh
  -> Quotes.refresh
  -> Account.evaluate/rebuild/subscriptions
  -> Agent.run

Agent
  -> Quotes   via adapters/agent_tools
  -> News     via adapters/agent_tools
  -> Account  via adapters/agent_tools

Account
  -> Quotes snapshot only, for valuation / order simulation / trigger evaluation
```

规则：

- Agent Runtime 是 application 层编排器，不是 bounded context，不拥有业务规则。
- News / Account / Quotes emit 的事件只表达事实；事件路由和后台 run 触发由 Agent Runtime 负责。
- Account 的 subscribed codes 由 Agent Runtime 注入 Quotes refresh scope。
- Quotes、News、Account 都不知道 Agent 存在。
- Agent 是消费者和决策者，不是三个执行模块的依赖。
- Account 可以读取 Quotes snapshot，但不调用行情 provider。
- News 不反向调用 Quotes / Account / Agent。

---

## 4. Spec 规则

模块 spec 写领域模型、行为契约和验收标准，不写实现现状流水账。详细写作规范见 [spec-guidelines.md](spec-guidelines.md)。

每个模块 spec 只约束该 bounded context 自己拥有的数据、规则、接口和事件，不替其他模块规定行为。执行模块不定义 Agent 工具 schema，也不描述下游 run 流程；Agent 工具注册协议写在 `agent-infra-module.md`，Agent 如何消费模块能力、每类 run 使用哪些工具、跨模块事件路由、定时调度和订阅注入写在 `agent-runtime-module.md`。

跨模块共享类型不在各模块内重复定义；统一写在 [shared-types.md](shared-types.md)。

推荐结构：

```text
定位
责任边界
领域模型
数据流
对外接口
模块独有功能
验收标准 / 例子
不纳入范围
```

所有实现以最新 spec 为准。快速迭代期以直接落地最新设计为目标。
