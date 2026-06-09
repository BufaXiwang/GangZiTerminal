# Frontend Design

> 前端体验与视觉系统规范。所有前端开发以本文档为准，不得脱离规范自由发挥。
>
> 本文档层级为 **Spec-anchored + Guidance**（见 spec-guidelines.md §1）：
> 视觉 token、布局骨架、数据流模式和交互契约是约束；具体像素微调和动效细节是指导。

---

## §1 设计哲学

### 1.1 产品定位

**A 股研究 + 模拟交易学习终端。** 用户是学习型交易者，核心循环：

```
看行情 → 读资讯 → 与 Agent 对话 → Agent 下单模拟交易 → 复盘决策链 → 调整策略 → 下一轮
```

前端是 **Rust 应用的 UI 层**，不是独立前端应用。所有业务真源在后端，前端只做：
- 展示后端推送的数据
- 收集用户输入发给后端
- 流式渲染 Agent 回复

### 1.2 核心原则

| 原则 | 含义 | 反例 |
|---|---|---|
| **决策优先** | 所有 UI 服务于「看→想→做→验」的决策循环 | 为好看加装饰性动画 |
| **信息密度** | 桌面端不是手机；在合理的密度下展示尽量多的有效信息 | 大面积留白、一屏只放一个卡片 |
| **数据即装饰** | 涨跌色、行业热度色块、K 线本身就是视觉节奏；不需要额外装饰元素 | 给每个卡片加渐变背景和阴影 |
| **克制** | 不加不需要的东西；每个元素都要能回答「用户决策时需要这个吗？」 | 加 loading 骨架屏、加欢迎引导、加成就系统 |
| **一致** | 同类元素在所有页面表现相同 | 行情在市场页红涨绿跌，在账户页反过来 |

### 1.3 气质

**专业工具感，不是消费品光泽。**

像 Bloomberg Terminal 的克制 + 好的排版。不追求「哇好漂亮」，追求「信息清晰、用起来顺手」。暖色调纸面底色避免长时间看屏的视觉疲劳，但不是为了「温馨」——是为了可读性。

---

## §2 视觉系统

### 2.1 色彩

所有色值通过 CSS custom properties 定义在 `:root`。组件和页面**不得硬编码色值**。

#### 背景

| Token | 值 | 用途 |
|---|---|---|
| `--bg-app` | `#faf6ee` | 全局底色（暖白纸面） |
| `--bg-card` | `#ffffff` | 卡片、弹窗、输入框底 |
| `--bg-soft` | `#f3ede0` | 分隔带、hover 背景、次级区域 |

#### 文字

| Token | 值 | 用途 |
|---|---|---|
| `--fg-strong` | `#2f2618` | 标题、关键数字 |
| `--fg-default` | `#5c5042` | 正文 |
| `--fg-muted` | `#897866` | 二级文字、标签 |
| `--fg-faint` | `#b8a995` | 三级文字、placeholder、禁用态 |

#### 边框

| Token | 值 | 用途 |
|---|---|---|
| `--border-strong` | `#d8c9b3` | 分割线、选中边框 |
| `--border-default` | `#e5d9c5` | 默认边框 |
| `--border-soft` | `#efe7d7` | 淡分隔 |

#### 品牌

| Token | 值 | 用途 |
|---|---|---|
| `--brand` | `#8a6532` | 主操作、active 态、链接 |
| `--brand-strong` | `#5e4220` | 按钮 hover、重强调 |
| `--brand-soft` | `#ead9be` | 选中背景、badge 底色 |

#### 行情语义

| Token | 值 | 含义 |
|---|---|---|
| `--chart-up` | `#c0392b` | **红涨**（A 股语义） |
| `--chart-down` | `#1f8a47` | **绿跌** |

所有展示涨跌的地方必须统一使用这两个 token。正收益 = 红，负收益 = 绿，零 = `--fg-muted`。

#### 状态

| Token | 值 | 用途 |
|---|---|---|
| `--state-warn` | `#c98a1e` | 警告（熔断、stale 数据、触发器） |
| `--state-info` | `#5e7b9a` | 信息提示 |
| `--state-stale` | `#b8a995` | 过期数据标记 |

### 2.2 字体

| Token | 栈 | 用途 |
|---|---|---|
| `--font-body` | Inter, -apple-system, PingFang SC, sans-serif | 正文、按钮、标签 |
| `--font-display` | Source Serif 4, Georgia, serif | 页面标题（仅 section head） |
| `--font-mono` | IBM Plex Mono, SF Mono, Menlo, monospace | 价格、代码、数量、时间戳 |

数字展示规则：
- 价格、金额、百分比：`font-variant-numeric: tabular-nums` + `--font-mono`（等宽对齐）
- 股票代码：`--font-mono`
- 普通计数：`--font-body` 即可

#### 字号

| 场景 | 大小 | 行高 |
|---|---|---|
| 页面标题 | 20px, `--font-display`, 600 weight | 1.3 |
| 区域标题 | 15px, `--font-body`, 600 weight | 1.4 |
| 正文 | 14px | 1.5 |
| 紧凑正文 | 13px | 1.4 |
| 小标签 | 12px | 1.3 |
| 指标大数字 | 24–28px, `--font-mono`, 600 weight | 1.2 |

### 2.3 间距与圆角

基础间距单位 4px，所有间距为 4 的倍数。

| Token | 值 | 用途 |
|---|---|---|
| `--radius-sm` | 4px | chip、小按钮 |
| `--radius-md` | 8px | 卡片、输入框 |
| `--radius-lg` | 12px | 弹窗、大面板 |

| Token | 值 |
|---|---|
| `--shadow-sm` | `0 1px 2px rgba(47,38,24,0.05)` |
| `--shadow-md` | `0 2px 6px rgba(47,38,24,0.08)` |

阴影用量极少，只用在浮层（弹窗、下拉、context menu）。卡片之间用边框区分，不用阴影。

### 2.4 暗色模式

**不纳入当前范围。** 所有色值走 token 是为将来做准备，但不要求现在实现暗色主题。

---

## §3 全局布局

### 3.1 AppShell 骨架

```
┌─────────────────────────────────────────────────────┐
│ macOS titlebar overlay zone (28px)                  │
├──────┬──────────────────────────────────────────────┤
│      │                                              │
│  S   │           main-content                       │
│  i   │                                              │
│  d   │  ┌──────────────────────────────────────┐    │
│  e   │  │  PageShell                           │    │
│  b   │  │    SectionHead (可选)                │    │
│  a   │  │    workspace (flex:1, overflow:auto)  │    │
│  r   │  └──────────────────────────────────────┘    │
│      │                                              │
│ 72px │              flex: 1                         │
└──────┴──────────────────────────────────────────────┘
```

| 区域 | 规格 |
|---|---|
| Sidebar | 固定 72px 宽，全高，icon-only 导航 |
| main-content | `flex: 1`，内部滚动 |
| titlebar zone | macOS `titleBarStyle: overlay` 需要 28px 上边距让红绿灯按钮不被遮挡 |

### 3.2 Sidebar

**窄图标导航**，不展开文字。从上到下：

| 位置 | 内容 |
|---|---|
| 顶部 | Logo mark「G」（`--font-display`，24px，品牌色） |
| 导航区 | 5 个图标按钮，垂直排列，48px 触控区 |

导航项（固定顺序）：

| 图标 | 路由 | 含义 |
|---|---|---|
| TrendingUp | `/market` | 市场 |
| Newspaper | `/` | 资讯（默认落地页） |
| Wallet | `/account` | 模拟账户 |
| Bot | `/agent` | Agent |
| Settings | `/settings` | 设置 |

状态：
- 默认：`--fg-muted`
- hover：`--bg-soft` 背景 + `--fg-default` 图标
- active（当前页）：`--brand-soft` 背景 + `--brand` 图标

**落地页是资讯页（`/`）**，因为用户打开终端第一件事是看今天有什么新闻。

### 3.3 PageShell

页面级通用包装：

- padding: 24px
- 可选 SectionHead（标题 + 副信息 + 操作按钮）
- workspace: `flex: 1; overflow: auto`

SectionHead 高度固定 56px，避免页面抖动。

### 3.4 Keep-alive

页面使用 lazy mount + CSS `display: none` 隐藏策略：首次访问时 mount，切走后隐藏而非 unmount。保持页面内部状态（滚动位置、输入、展开态）。

---

## §4 页面设计

### 4.1 资讯页（NewsPage, `/`）

**目标：** 快速扫描今天和近期的资讯，发现值得深入研究的信号。

#### 结构

```
PageShell
├─ control strip (SectionHead 区域)
│    FTS搜索框 + 来源多选chips + 刷新按钮
├─ 日期导航条 (横向滚动)
│    [06-08] [06-07] [06-06] ... (最近14天，带新闻数)
└─ workspace: 时间线
     ┌─ 06-08 (sticky 日期头) ────────────────┐
     │  14:30  [新浪]  标题文本预览...           │
     │  13:15  [东方财富]  标题文本预览...       │
     ├─ 06-07 (sticky 日期头) ────────────────┤
     │  ...                                    │
     └─────────────────────────────────────────┘
```

#### 交互契约

| 交互 | 行为 |
|---|---|
| mount / filter 变化 | `fetchNews({ order: "desc", limit: 50 })` → 最新一页 |
| 下滑到底 | 加载更早：`publishedTo = oldest.publishedAt, order: "desc"` |
| 上滑到顶 | 加载更新：`publishedFrom = newest.publishedAt, order: "asc"` → reverse → prepend |
| 点击日期锚点 | 替换窗口：`publishedTo = 该日 23:59:59.999, order: "desc"` |
| 搜索 | 300ms debounce FTS，重置窗口 |

#### 性能要求

- DOM 窗口上限 **200 条**（超限裁远端，游标可回滚重拉）
- 新闻行使用 `content-visibility: auto`（屏外跳过布局 + 绘制）
- 向更新方向 prepend 时做滚动锚定补偿，视口不跳动
- 去重一律按 id Set（闭区间游标会带回边界条）

#### 新闻行样式

- 单行紧凑：时间 + 来源 badge + 标题（不折行，`text-overflow: ellipsis`）
- hover 背景 `--bg-soft`
- 来源 badge 使用 chip 样式，不同来源可有不同底色
- 正文 > 3 行时折叠，展开按钮

---

### 4.2 市场页（MarketPage, `/market`）

**目标：** 一屏看到市场全貌（指数、涨跌分布、行业热度），快速定位个股看 K 线。

#### 结构

```
PageShell (紧凑模式：标题行仅 lastUpdated + 全局刷新)
├─ metrics row (横向排列，等高卡片)
│    [上证] [深证] [创业板] [科创50] [涨跌分布] [行业热度]
└─ workspace (两栏)
     ├─ list panel (280px, 左)
     │    ├─ 排序 tabs: 涨跌 | 成交
     │    ├─ 类别 chips: 股票 | 指数 | 基金
     │    ├─ 搜索框
     │    └─ 列表 (react-window 虚拟滚动)
     └─ detail panel (flex:1, 右)
          ├─ 标的 header (name + code + 实时报价)
          ├─ 周期 tabs: 日 | 周 | 月 | 1m | 5m | 15m | 30m | 60m
          └─ K线图 (klinecharts)
```

#### 指数卡片 (IndexCard)

- 标题：指数名称
- 主数字：当前点位，`--font-mono`, 24px
- 涨跌：绝对值 + 百分比，涨跌色
- 迷你日 K sparkline（可选，不强制）

#### 涨跌分布卡片 (BreadthCard)

- 标题：市场宽度
- 横向堆叠条：涨停（深红）| 涨（红）| 平（灰）| 跌（绿）| 跌停（深绿）
- 数字标注：上涨 N / 下跌 N / 平盘 N

#### 行业热度卡片 (HeatmapCard)

- 标题：行业热度
- top 3 涨 + top 3 跌，色块 + 百分比

#### 列表项样式

- 双行紧凑：行 1: 名称 + 代码 | 行 2: 价格 + 涨跌% + 成交额
- 价格和百分比使用 `--font-mono` + tabular-nums
- 涨跌色全局一致
- 自选标记：⭐ 在名称前
- hover / selected 背景

#### K 线面板

- 默认选中 `000001.SH`（上证指数，有预热数据）
- 周期切换：重新请求 `ensureChartData`
- 无数据状态：显示「暂无 K 线数据」提示
- 深度历史：`extendChartHistory` 延伸加载
- K 线颜色遵循 `--chart-up` / `--chart-down`

#### 数据流

- 列表一次性加载（`listMarket({ limit: 10000 })`，universe ~7500 条，内存 + DB 读）
- 行情刷新走 `market-quotes-refresh-progress` 事件，**2.5s 节流**
- 缓存策略：stale-while-revalidate（30s cache，miss = loading，stale = 静默后台刷新）

---

### 4.3 模拟账户页（AccountPage, `/account`）

**目标：** 清楚看到「我有多少钱、持了什么仓、赚还是亏」，管理自选股。

#### 结构

```
PageShell (紧凑模式)
├─ AccountSummary (全宽概览条)
│    总资产 | 现金 | 持仓市值 | 已实现盈亏 | 未实现盈亏 | 持仓数 | ...
└─ workspace (两栏)
     ├─ main (flex:1, 左)
     │    ├─ PositionsPanel (持仓表)
     │    └─ 选中持仓的 K 线 (可选展示)
     └─ side (280px, 右)
          └─ WatchlistPanel (自选股列表)
```

#### AccountSummary

- 横向一行关键指标，指标大数字 + 标签
- 总资产和盈亏使用涨跌色
- 可选 grid 展开更多字段

#### PositionsPanel

- 表格布局：代码 | 名称 | 数量 | 成本 | 现价 | 盈亏 | 盈亏% | 保护条件
- 保护条件（止损/止盈/时间止损）以 chip 形式展示在行内
- 空仓时显示空状态文案
- 点击行 → 下方展示该股 K 线

#### WatchlistPanel

- 紧凑列表：名称 + 代码 + 最新价 + 涨跌%
- 操作：添加（弹窗输入代码）、移除、编辑备注
- 点击行 → 弹出 K 线 modal
- 与市场页共享 `watchlistStore`（⭐ 同步）

#### 重置账户

- 「重置账户」按钮在页面操作区
- 点击弹出确认对话框：说明将清空持仓/订单/成交，保留自选股
- 确认后调 `accountReset()`
- 可查看历史归档：`listAccountArchives()`

#### 红线

- **用户没有交易写入口**（spec §4：人工 UI 不能下单/调仓），只能管自选
- 所有交易由 Agent 通过 `operate_account` 工具执行

---

### 4.4 Agent 页（AgentPage, `/agent`）

**目标：** 与 Agent 对话、管理投资策略、审阅决策链。

#### 结构

```
PageShell (minimal header)
├─ left panel (240px)
│    ├─ Runs 列表 (近期 run，mode badge + 状态)
│    ├─ 复盘报告列表
│    └─ 资讯自动分析开关
├─ center panel (flex:1)
│    ├─ 对话区 (message bubbles，滚动)
│    │    [user bubble]
│    │    [assistant bubble, streaming...]
│    └─ 输入区
│         textarea + 发送按钮 + 图片附件
└─ right panel (280px)
     ├─ 投资策略面板 (查看/编辑)
     ├─ 分析结果列表 (AnalysisResult)
     └─ 熔断状态条
```

#### 对话区

- 角色区分：用户消息右对齐、品牌底色；Agent 消息左对齐、白底
- 流式渲染：`listen("agent-event")` 增量 text_delta → 逐 token 追加到当前 assistant bubble
- markdown 渲染（后续需求，当前纯文本即可）
- 图片支持：用户可附带图片（data-URL → 后端 PayloadStore）
- 发送：Ctrl/⌘+Enter 发送；运行中 Ctrl+C 停止当前 run
- **非阻塞 pending 队列**：run 进行中输入框**不锁定**；此时提交的消息不打断当前对话，而是以「排队中」气泡显示在对话区，当前 run 终态（`done` 事件）后**自动逐条发出**（对齐 Claude Code 体验）。排队消息可在发出前移除。run 收尾按 assistant 消息 id 精确定位（`runActiveRef`），保证流水线多轮不串台、不重演「晚到 text_delta 被丢弃」的 race。纯前端编排，不改 `agent_send_message` 后端契约（同会话仍串行执行）。

#### 投资策略面板

- 显示当前策略文本 + 版本号 + 状态（active/draft/archived）
- 编辑模式：textarea，保存时 `agentUpsertStrategy`
- 策略是自然语言描述（不是结构化参数）
- 版本历史可查看（`includeHistory: true`）

#### Runs 列表

- 每行：mode badge（对话/资讯/账户触发/复盘）+ 状态 + 时间
- mode badge 颜色按类型区分
- 点击 run → 在对话区展示该 run 的消息历史（未来需求）

#### 分析结果列表

- 每行：相关标的 + action/no_action 判定 + 简要理由
- 新结果通过 `agent-analysis-result` 事件实时追加

#### 熔断状态条

- 熔断激活时：顶部或右侧显示警告条（`--state-warn` 底色）
- 显示：熔断原因 + 「恢复交易」按钮
- 恢复需要确认

#### 资讯自动分析

- 开关 toggle：`agentSetNewsAutoAnalysis(enabled)`
- 开启后 Agent 自动分析新入资讯（news buffer → news mode run）

---

### 4.5 设置页（SettingsPage, `/settings`）

**目标：** 配置 LLM 服务商通道（让 Agent 能对话）。

#### 结构

```
PageShell
├─ 当前模型选择器 (CurrentModelSelector)
│    已配置 channel 的单选列表
├─ 添加通道 (AddChannelForm)
│    ├─ 快速预设 chips (DeepSeek / OpenAI / Anthropic Relay)
│    └─ 自定义表单: wire format + base URL + API key + model
└─ 通道列表 (ChannelList)
     每行: channel name + model + 状态 + 编辑/删除
```

#### 安全红线

- API key **永远不回显**，列表只显示 `apiKeySet: boolean`
- 编辑时 API key 字段为空表示保留原值
- API key 通过 Tauri secure storage 持久化，不写入 SQLite 明文

---

## §5 通用组件契约

### 5.1 按钮 (`.btn`)

| 变体 | 样式 | 用途 |
|---|---|---|
| default (ghost) | 透明底 + 文字色边框 | 次要操作 |
| primary | `--brand` 底色 + 白字 | 主操作（每个视图最多 1 个） |
| danger | `--chart-up` 底色 + 白字 | 破坏性操作（删除、重置） |
| icon-only | 无边框 + icon | 工具栏操作 |

状态：hover（加深底色 5%）、focus（`outline: 2px solid var(--brand)`）、disabled（opacity 0.5）。

尺寸：默认 32px 高、padding 8px 16px；compact 28px 高。

### 5.2 Chip (`.chip`)

小型标签/筛选器：

- 默认：`--bg-soft` 底 + `--fg-muted` 文字
- active：`--brand-soft` 底 + `--brand` 文字
- 圆角 `--radius-sm`
- 高度 24px，padding 4px 8px，font-size 12px

### 5.3 输入框 (`.input`)

- `--bg-card` 底色
- `--border-default` 边框，focus 时 `--brand` 边框
- `--radius-md` 圆角
- 高度 36px，padding 8px 12px
- placeholder 使用 `--fg-faint`

### 5.4 状态点 (`.status-dot`)

行内小圆点（8px），用于表示在线/告警/错误等状态：

| 状态 | 颜色 |
|---|---|
| ok | `--chart-down` (绿) |
| warn | `--state-warn` |
| error | `--chart-up` (红) |
| loading | `--state-info` + pulse 动画 |
| stale | `--state-stale` |

### 5.5 表格

- 表头：`--fg-muted` 文字，12px，`text-transform: uppercase` 可选
- 行高：40px（默认）或 32px（紧凑）
- hover：`--bg-soft` 背景
- 选中行：`--brand-soft` 背景 + 左侧 2px `--brand` 边框
- 数字列右对齐，`tabular-nums`
- 不使用斑马纹（边框分隔足够）

### 5.6 弹窗 / Modal

- 居中显示，backdrop `rgba(47,38,24,0.3)`
- `--bg-card` 底色，`--radius-lg` 圆角
- `--shadow-md` 阴影
- 标题 + 内容 + 底部按钮区（右对齐：取消 + 确认）
- ESC / 点击 backdrop 关闭

### 5.7 Context Menu

- 右键触发，绝对定位
- `--bg-card` 底色，`--shadow-md`
- 每项 32px 高，hover `--bg-soft`
- 支持分隔线

### 5.8 空状态

当列表 / 面板无数据时：

- 居中显示图标（lucide icon，48px，`--fg-faint`）+ 一行文案
- 不用插画、不用大面积留白
- 文案示例：「暂无持仓」「暂无新闻」「尚未配置服务商」

### 5.9 加载状态

- **不使用骨架屏**（过度设计，且数据通常 <1s 返回）
- 首次加载：居中 spinner（简单 CSS 旋转圆环，16px，`--brand`）
- 后台刷新：不显示加载态（stale-while-revalidate）
- 按钮操作中：按钮内 spinner + disabled

---

## §6 数据流模式

### 6.1 核心原则

```
前端不持有业务真源。
所有业务状态（持仓、报价、订阅、策略）= Rust 真源 → event 推送 → 前端缓存。
```

| 数据流 | 机制 |
|---|---|
| 查询 / 命令 | `invoke('cmd', args)` → **走 specta 生成的强类型 wrapper**，不裸调 `invoke` |
| 流式数据 | Rust `app.emit(event, payload)` → 前端 `listen(event)` |
| 取消长任务 | Rust 持有 `CancellationToken`，前端通过另一 command 触发取消 |
| 业务状态 | Rust 真源 → event 推送 → zustand 缓存 |
| UI 状态 | 留 React 本地 state |

### 6.2 红线

- 前端**不做业务计算**（PnL / 估值 / 信号）——由 Rust 算好推过来
- 前端**不直接发外部 HTTP**——所有 provider 调用走 Rust
- 前端**不裸调 `invoke`**——只使用 `bindings.ts` 导出的强类型函数
- 前端**不做乐观更新**——命令成功后才更新 UI

### 6.3 状态管理

| 类别 | 方案 |
|---|---|
| 跨页面共享业务缓存 | zustand store（例：`watchlistStore`） |
| 页面内数据 | React `useState` / `useRef` |
| 表单输入 | React `useState`（受控组件） |

**不使用 Redux / Redux-Toolkit / Context 做状态管理。** zustand 只用于真正需要跨页面共享的数据。

### 6.4 事件监听

前端监听的 Tauri 事件：

| 事件 | 来源 | 前端行为 |
|---|---|---|
| `market-quotes-refresh-progress` | Quotes | 行情刷新进度，2.5s 节流后 refetch list |
| `agent-event` | Agent Runtime | 流式 text_delta → 追加到 assistant bubble |
| `agent-run-finished` | Agent Runtime | 刷新 Agent state |
| `agent-analysis-result` | Agent Runtime | 追加分析结果到列表 |

---

## §7 性能契约

| 指标 | 目标 |
|---|---|
| 页面首次有意义渲染 | < 500ms（keep-alive 复访 < 50ms） |
| 行情列表渲染 7500 行 | 虚拟滚动，可见区域外不渲染 |
| 新闻时间线 | DOM 上限 200 条，`content-visibility: auto` |
| 行情刷新重渲染 | 2.5s 节流，不逐条触发 |
| K 线图切换周期 | < 300ms 显示已缓存数据，后台拉取最新 |
| 前端 JS bundle | 关注但不强制 500KB 限制（桌面端，非 web） |

### 7.1 Keep-alive 策略

页面 lazy mount + `display: none` 隐藏。首次访问 mount，切走隐藏不卸载。保持：
- 滚动位置
- 输入框内容
- 展开/折叠状态
- K 线图实例

### 7.2 虚拟滚动

长列表（市场页 > 100 行、新闻页 > 50 行）使用 `react-window` 虚拟化。

### 7.3 预热

App 启动时预热核心指数日 K（`000001.SH` day），避免首次打开市场页白屏等待。

---

## §8 可访问性

| 要求 | 说明 |
|---|---|
| 语义化 HTML | `<nav>`, `<main>`, `<section>`, `<table>`, `<button>` |
| 键盘导航 | 所有可交互元素可 Tab 到达 |
| ARIA 标签 | sidebar 导航、context menu、modal |
| 焦点指示器 | `outline: 2px solid var(--brand)`，不用 `outline: none` |
| 颜色对比度 | 正文文字与背景 contrast ratio >= 4.5:1 |

不要求 screen reader 完整适配（桌面专业工具，目标用户群体）。

---

## §9 技术约束

### 9.1 技术栈（已锁定）

| 库 | 版本 | 用途 |
|---|---|---|
| React | 19 | UI 框架 |
| Vite | 6 | 构建 |
| TypeScript | 5.6+ | 类型安全 |
| React Router | 7 | HashRouter 路由 |
| react-window | 2 | 虚拟滚动 |
| klinecharts | 9 | K 线图 |
| lightweight-charts | 5 | TradingView 图表（备用 / sparkline） |
| lucide-react | latest | 图标 |
| zustand | 5 | 轻量状态管理 |
| tauri-specta | — | Rust → TS 类型生成 |

### 9.2 路由

HashRouter（`/#/market`），因为 Tauri 使用 `file://` 协议加载前端，不支持 `pushState`。

### 9.3 类型安全

- 所有 Tauri command 调用走 `src/bindings.ts`（specta 自动生成）
- 不手写 `invoke` 调用
- DTO 类型从 bindings 导入，不自行定义

### 9.4 文件组织

```
src/
├─ App.tsx              # 路由 + keep-alive mount
├─ main.tsx             # entry point
├─ bindings.ts          # specta 生成（不手动编辑）
├─ styles.css           # 全局样式 + design tokens
├─ components/          # 跨页面共享组件
│    AppShell.tsx
│    PageShell.tsx
│    SectionHead.tsx
│    Sidebar.tsx
│    KlineCanvas.tsx
│    KlineModal.tsx
├─ lib/                 # 工具函数 + hooks + stores
│    router.ts
│    watchlistStore.ts
│    quotePull.ts
│    beijingTime.ts
│    newsWindow.ts       # 双向窗口纯函数
│    scrollAnchor.ts
│    marketListCache.ts
│    perfLog.ts
│    useCoreIndexes.ts
│    useKlineData.ts
│    useMarketBreadth.ts
│    useIndustryHeatmap.ts
│    tradingSession.ts
├─ pages/               # 页面组件
│    NewsPage.tsx
│    MarketPage.tsx
│    AccountPage.tsx
│    AgentPage.tsx
│    SettingsPage.tsx
│    news/              # 页面内子组件
│    market/
│    account/
│    settings/
```

规则：
- 页面内子组件放 `pages/<page>/` 目录下
- 跨页面组件放 `components/`
- 数据 hooks 和工具函数放 `lib/`
- 不建立 `utils/`、`helpers/`、`services/` 等模糊目录

---

## §10 开发红线

| 禁止 | 原因 |
|---|---|
| 在前端做业务计算 | Rust 是真源，前端只展示 |
| 直接发 HTTP 请求 | 所有外部调用走 Rust provider |
| 裸调 `invoke()` | 用 specta 生成的 `commands.*` |
| 使用 Redux | zustand 足够，不需要 Redux 的复杂度 |
| 引入 CSS-in-JS | 项目使用全局 CSS + token |
| 引入 UI 框架 (MUI/Ant Design/Chakra) | 自建组件系统，保持轻量和一致性 |
| 硬编码色值 | 使用 CSS variables |
| 添加 emoji 到 UI | 专业工具感，不用 emoji（代码注释中也不加） |
| 乐观更新 | 命令成功后才更新 UI |
| 引入 i18n | 当前只做中文 |

---

## §11 不纳入当前范围

- 暗色模式
- 响应式 / 移动端适配（桌面专用）
- Screen reader 完整适配
- 多语言 i18n
- 可拖拽面板布局
- 可定制快捷键
- 欢迎引导 / onboarding
- 通知中心 / toast 系统（简单操作反馈用按钮状态即可）
