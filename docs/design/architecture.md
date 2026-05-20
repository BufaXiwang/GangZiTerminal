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
| Agent | [agent-module.md](agent-module.md) | 决策 loop、工具调用、记忆、复盘、自迭代策略 |

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
Agent
  -> Quotes   via adapters/agent_tools
  -> News     via adapters/agent_tools
  -> Account  via adapters/agent_tools

Account
  -> Quotes snapshot only, for valuation / order simulation / trigger evaluation
```

规则：

- Quotes、News、Account 都不知道 Agent 存在。
- Agent 是消费者和决策者，不是三个执行模块的依赖。
- Account 可以读取 Quotes snapshot，但不调用行情 provider。
- News 不反向调用 Quotes / Account / Agent。

---

## 4. Spec 规则

模块 spec 写领域模型、行为契约和验收标准，不写实现现状流水账。

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
