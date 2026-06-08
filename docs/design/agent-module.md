# Agent Specs

> Agent 相关 spec 已拆成 Infra 和 Runtime 两部分。本文档只作为入口，不重复定义契约。

## 一句话定位

Agent 由两层组成：

- [Agent Infra](agent-infra-module.md)：LLM Agent 执行底座，负责消息、Provider / 模型渠道、上下文管理、工具注册 / 调用协议、ToolCall 审计和基础 loop。
- [Agent Runtime](agent-runtime-module.md)：产品里的 Agent 应用层，负责跨模块事件编排、定时任务、订阅行情注入、触发 Agent run、运行 mode（对话 / news / 复盘）、工具使用策略、决策审计、投资策略和复盘。

## 边界

- Agent Infra 是 Agent 的执行基础设施，不拥有投资判断、投资策略或交易记录。
- Agent Runtime 是 Agent 的业务运行期，拥有 `AgentRun`、`InvestmentStrategy`、`AnalysisResult`、`AgentTrade`（复盘产物为 workspace 文件）。
- Quotes / Account / News 不 import Agent Infra 或 Agent Runtime；它们只暴露事实、事件和 facade。
- Agent Runtime 可以调用各模块公开 facade，但不拥有成交规则、资讯抽取或行情 provider 规则。
