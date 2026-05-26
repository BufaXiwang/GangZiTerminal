# AGENTS.md

> 这是给"读代码的 agent"的入口页。保持短——所有详细规约在 `docs/design/architecture.md` 里。

## Project

A 股研究 + 模拟交易学习终端。Agent 自驱动：从市场数据 + 资讯识别机会 → 在模拟账户里实盘验证 → 沉淀成可审计、可复盘的判断链。

**不连真券商，只做模拟。**

## Read First

- **[docs/design/architecture.md](docs/design/architecture.md)** ← 整体设计入口。模块级领域契约以 `docs/design/*-module.md` 为准
- [docs/design/](docs/design/) — 各模块 spec。写实现前先确认对应 spec；spec 只写最新设计契约
- [docs/design/spec-guidelines.md](docs/design/spec-guidelines.md) — spec 写作规范：定位 / 边界 / 领域模型 / 能力 / 验收标准
- [docs/design/shared-types.md](docs/design/shared-types.md) — 跨模块共享领域类型
- [docs/design/references/](docs/design/references/) — provider / 渠道 / 模型 wire format 参考
- [docs/development.md](docs/development.md) — 开发命令 / Tauri runtime / 本地配置
- [docs/frontend-design.md](docs/frontend-design.md) — 前端体验 / 视觉系统 / 展示边界
