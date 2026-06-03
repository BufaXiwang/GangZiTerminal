# Frontend Design Spec

> 本文档是前端体验和视觉系统的设计契约。模块级领域契约仍以 `docs/design/*-module.md` 为准。
>
> 本文档只描述最新设计目标和约束，用于后续页面、组件、交互改造。

## 一句话定位

前端是 **A 股研究 + 模拟账户 + Agent 决策闭环的工作台**，不是营销页，也不是通用聊天壳。

界面要帮助用户快速回答三个问题：

```text
现在市场发生了什么？
Agent 为什么这样判断 / 操作？
模拟账户验证结果如何？
```

---

## 1. 设计原则

### 决策优先

- 页面首屏优先展示会影响判断和操作的内容：市场状态、账户风险、Agent 当前动作、关键新闻。
- 详情、历史、解释、调参入口放在下钻区域，不挤占核心视野。
- 每个页面最多突出 1 个主任务；其余内容作为上下文辅助。

### 数据密集但克制

- 本软件是金融工作台，信息密度可以高，但视觉噪声要低。
- 优先使用列表、表格、分栏、时间线、指标条，不使用大面积 hero、插画、营销式卡片堆叠。
- 一屏内模块数量保持可扫描；超过 5-9 个同级模块时，应分组、折叠或下钻。

### 实时性可见

- 行情、新闻、账户、Agent run 都必须暴露更新时间或 freshness。
- stale / loading / partial / failed 状态要直接可见，不能只在 console 里报错。
- 涉及交易判断的界面必须显示数据是否新鲜。

### 过程可审计

- Agent 的工具调用、读取数据、生成判断、执行账户动作都要在 UI 中可见。
- 后台 run 不能静默消失，应进入可追溯的 timeline / episode 列表。
- 用户应能从一次账户动作追溯到 Agent 判断、工具结果和当时的市场事实。

### 沿用现有视觉系统

- 保留当前暖白纸面风格、窄侧栏、Lucide 图标、CN 红涨绿跌语义。
- 不引入新的 UI 组件库，除非先形成单独设计决策。
- 新组件优先复用 `page-shell`、`section-head`、`ghost`、`primary`、`chip`、`segmented`、`switch`、`agent-card` 等既有模式。

---

## 2. 视觉系统

### 色彩

使用 `src/styles.css` 里的 CSS variables 作为唯一色彩入口：

- 背景：`--bg-app`、`--bg-card`、`--bg-soft`
- 文字：`--fg-strong`、`--fg-default`、`--fg-muted`、`--fg-faint`
- 边框：`--border-strong`、`--border-default`、`--border-soft`
- 品牌：`--brand`、`--brand-strong`、`--brand-soft`
- 行情：`--chart-up`、`--chart-down`

规则：

- 不在组件里硬编码大段新颜色；动态涨跌、状态色也应落到 tone class 或 token。
- A 股行情语义固定为 **红涨绿跌**。
- 同一页面不能被单一色相统治；暖白底色是基础，不再叠加大面积渐变、光斑、装饰图形。

### 字体和数字

- 正文使用 `--font-body`。
- 标题可以使用 `--font-display`，但工作台内标题不做 hero 级放大。
- 金额、价格、涨跌幅、成交量、代码、时间优先使用 tabular / mono 风格，保证纵向可比较。
- 中文 UI 文案保持短句，避免解释软件功能的长段说明。

### 间距和圆角

- 基础间距使用 4 / 8 / 12 / 16 / 24 px 阶梯。
- 密集列表和工具栏优先用 6-8px gap。
- 新增卡片默认使用 `--radius-sm`；只有沿用现有容器或 modal 时使用更大圆角。
- 不新增卡片套卡片；重复项可以是 card，页面 section 不做漂浮卡片。
- **例外——配置/设置类页面**（如设置页）：内容是分组的配置项而非数据工作面，**允许**把页面 section 做成 `radius-lg` + 轻 `shadow-sm` 的卡片来分组（见 §4 设置页 视觉结构）。数据工作面（市场 / 资讯 / 模拟账户）仍保持扁平、不漂浮。

### 图标

- 操作按钮使用 `lucide-react` 图标。
- 常见动作优先用图标 + tooltip / title：刷新、搜索、筛选、排序、全屏、关闭、保存、删除。
- 只有语义不清或高风险动作需要图标 + 文本。

---

## 3. 全局布局

### App Shell

```text
72px sidebar
  -> main content
    -> main-scroll
      -> page-shell
```

规则：

- 侧栏保持窄图标导航，不在侧栏塞复杂状态。
- 主内容区是工作区，不做营销页头。
- 页面高度和滚动边界必须清晰；避免多个无意义嵌套滚动容器。

### 页面骨架

标准页面结构：

```text
page-shell
  section-head: 标题 + 状态/更新时间 + 主操作
  optional control strip: filter / search / segment / refresh
  workspace: list + detail / chart + table / timeline + inspector
```

规则：

- 标题区只放当前页面的操作状态和少数主操作。
- 筛选、排序、搜索放在 control strip，不能散落在不同卡片里。
- 数据列表和详情应保持同屏联动，减少页面跳转。

### 响应式

- Desktop 首要优化宽度：1280 / 1440 / 1728 px。
- 窄屏优先保持可读和可操作，不追求完整多列并排。
- 固定格式元素，如行情行、K 线容器、工具栏、按钮、标签，要有稳定尺寸，不能因内容变化导致布局跳动。

---

## 4. 核心页面模式

### 市场页

目标：快速扫市场、筛候选、下钻标的。

推荐结构：

```text
市场状态 / 核心指数 / breadth
  -> 筛选和排序
  -> 股票 / 指数 / 基金列表
  -> 选中标的详情 + KLineChart
```

规则：

- 列表必须能按涨跌幅、成交额、量比、PE/PB、类别、是否自选过滤或排序。
- 每个标的最少展示：名称、代码、类别、价格、涨跌幅、成交额、更新时间。
- 空值显示 `-`，不要显示 `null` / `undefined` / `NaN`。
- 行情列表使用紧凑行，不用大卡片堆叠。

### 资讯页

目标：按时间和影响快速阅读新闻。

推荐结构：

```text
更新时间 / 刷新状态
  -> 来源 / 日期 / 关键词筛选
  -> 时间线
  -> 展开正文 / 关联标的 / Agent 分析入口
```

规则：

- 时间线适合资讯流；默认倒序。
- **日期导航 = 双向窗口流**：列表是全局倒序时间线的一段连续窗口。点顶部任意一天 → 以那天为锚做一次干净加载（取那天最新一页），快速定位；**往上滑加载更新、往下滑加载更早**，两端都用 keyset 游标按需取**一页**（见 news-module.md `fetch_news`「双向 keyset 读取」）。**严禁**为定位某天而把它与当前位置之间的所有天全量拉取（高频源单日可达数千条，会卡顿且可能定位不到）。长列表渲染用 `content-visibility` 等原生窗口化，屏外行不绘制。
- 每日真实条数取后端 `dateCounts`，不用分页累积的 items 计数。**`dateCounts` 只由无游标请求（初始/筛选/刷新）维护**——带 keyset 游标的窗口翻页/锚定请求返回的 `dateCounts` 被时间游标截断，必须忽略，否则越往回滑日期计数越少、较新的天会错误地变 0 不可点。
- **窗口有界**：保留的 items 有上限（滑动窗口），向一端扩展超限即裁掉远端（被裁端可回滚重拉），裁剪/prepend 都做滚动锚定。避免数千常驻 DOM 节点导致切 tab / 滚动卡顿（`content-visibility` 只省绘制不省布局重算）。详见 news-module §`fetch_news` 双向 keyset 读取。
- 新闻正文可渐进展开，避免首屏塞满长文本。
- 关联标的、重要性、Agent 是否已分析要成为可扫描字段。
- 资讯行支持右键浮层：「用浏览器打开原文」「复制链接」。原文走 Rust `open_external`
  命令调用系统默认浏览器（仅放行 http/https），前端不裸调 plugin invoke。无 url 的条目禁用。
- 全局屏蔽 WebView 原生右键菜单（后退/重载/检查），改由各列表自定义浮层接管；
  输入框 / 文本域 / contenteditable 例外，保留原生复制粘贴菜单。

### 模拟账户页

目标：看清账户状态、持仓风险、Agent 操作后果。

推荐结构：

```text
账户总览
  -> 自选列表
  -> 风控提醒
  -> 当前持仓 + 选中持仓 KLineChart
  -> 最近平仓 / 复盘入口
```

规则：

- 账户金额、仓位、盈亏、风险状态必须优先展示。
- 用户只管理自选和查看账户，不提供人工交易入口。
- 持仓行必须能追溯到开仓 episode / 策略 / 止损止盈条件。

### Agent 页

目标：观察 Agent 的思考、工具调用、决策和复盘。

推荐结构：

```text
Agent tabbar
  -> 对话流 / run timeline
  -> 策略卡
  -> 启发式 / 复盘 / episode
```

规则：

- Chat 不是唯一入口；后台 run、复盘、策略卡同样是一等信息。
- 工具调用使用 timeline row：工具名、输入摘要、输出摘要、耗时、错误状态。
- Agent 产生账户动作时，必须有醒目的 episode / intent / account result 链接。

### 设置页

目标：管理 Agent 模型渠道（服务商连接 + 模型）。契约见 [agent-infra-module.md §2](agent-infra-module.md) `ProviderChannel`。

推荐结构（两栏吃满宽，桌面端一屏不滚）：

```text
当前模型（顶部全宽）：channel 选择器，单选，run 走选中渠道 —— 唯一的「设为当前」入口
两栏：
  左 模型渠道（只读管理）：每个保留模型一行（avatar + {model} / {渠道名} + 消息格式 + host + key 状态 + 删除）
  右 添加渠道：[快速预设 | 自定义]
       快速预设：选 DeepSeek/OpenAI/Anthropic 官方 → 只填 API Key
       自定义：渠道名 + 消息格式(Messages/Chat Completions/Responses) + Host + API Key
       → 保存后自动发现模型 → 勾选确认保留（发现失败则手动输入一个/多个模型名确认）
```

规则：

- 渠道按**消息格式**抽象，不按厂商写死；「渠道名」即展示用 provider name，列表与当前模型选择器都用 `{model} ({渠道名})` 文案。
- **职责分离，避免重复**：「当前模型」是**唯一**的 active 切换入口（选哪个渠道跑 run）；「模型渠道」是**只读管理**视图（看配置 / 删除），**不再标「当前」徽章、不做 active 行高亮**——active 状态只在「当前模型」里表达，两块各司其职不重复标记。
- API Key **只提交不回显**：保存后列表只显示"已配置"状态，不回传明文（走 specta 强类型 command，不裸调 invoke，不在前端持有 token）。
- 模型发现失败时降级为手动输入模型名（允许多个），不阻塞配置。
- 新建渠道默认 enabled；首次配置完成后自动设为当前模型（若此前没有当前模型）。

视觉结构（沿用暖纸面设计系统，不引入新色）：

- **布局吃满内容区宽度**（max-width ~1280）：顶部「当前模型」全宽；其下「模型渠道」「添加渠道」并排两栏（窄屏 < ~1024 自动堆叠为单栏）。目标是桌面端一屏放下、不产生页内竖滚。
- 区块各为一张卡片（`bg-card` + `border-soft` + `radius-lg` + `shadow-sm`）：header 条（serif 标题 + 右侧 muted hint/计数）+ body。模型渠道的行 full-bleed（行间 `border-soft` 分隔）。
- provider 头像：取渠道名首字母的方形 `brand-soft` 徽标，用在渠道行最左与预设卡片左侧，给来源一个视觉锚点。
- 当前模型选择器用一排 channel pill 卡片（model mono + provider muted）；选中态为 `brand` 描边环 + `brand-soft` 底 + 右上角 brand Check，明显区别于未选中。
- 渠道行 key 状态用状态点 + 文字：已配置 = `chart-down` 绿点，未配置 = `fg-faint` 点。

---

## 5. 数据展示组件

### 列表和表格

适用：市场全列表、扫描结果、订单、持仓、策略卡、复盘记录。

规则：

- 结构化可比较数据优先用 table-like list，不用自由排版卡片。
- 支持搜索、筛选、排序、分页或虚拟列表；大列表不能一次渲染全部复杂节点。
- 数字右对齐，名称左对齐，状态和操作列保持不换行。
- 空值显示 `-`。
- 行 hover、selected、active、stale 状态要有明确视觉差异。

### 卡片

适用：账户摘要、策略卡、复盘摘要、风险提醒。

规则：

- 卡片用于承载一个独立对象，不用于包裹整个页面 section。
- 卡片最多突出 1 个主值，其他字段降级为 meta。
- 卡片内容过长时截断或折叠，详情放 inspector / modal / expanded row。

### 时间线

适用：新闻、Agent run、工具调用、账户事件、复盘链。

规则：

- 时间线默认倒序，最新事件在上。
- 每条事件显示时间、来源、状态和主内容。
- 系统事件、工具事件、账户事件、用户消息要用不同 tone，但不能过度彩色。

### K 线图

K 线图统一使用 **lightweight-charts** 库（TradingView 出品，已锁定在 `package.json`）。**不包 wrapper 组件，页面直接调 lightweight-charts API**。

规则：

- 使用 lightweight-charts 的 `createChart` + `addCandlestickSeries` / `addLineSeries` / `addHistogramSeries` 等原生 API。
- K 线和成交量分两个 series（candle + histogram），通过 priceScale / pane 分离展示（参考 lightweight-charts docs/panes）。
- 周期切换器（分时 / 1m / 5m / 15m / 30m / 60m / 日 / 周 / 月，默认日 K）作为页面 control strip 的一部分，切换时调对应后端 command 重拉数据并 `setData()`。
- 页面层负责为图表提供稳定容器尺寸（一般 fixed height，例如 480 / 600px），不能让图表因列表切换或按钮 hover 发生高度跳动。
- 涨跌颜色按 CSS variables `--chart-up` / `--chart-down` 配置（A 股语义红涨绿跌）。
- 不同页面需要 K 线时各自调 lightweight-charts；如果出现明显重复逻辑（例如周期切换 + 数据拉取 + 错误态），允许在 `src/lib/` 抽 hook（如 `useKlineData`），但不抽完整 UI 组件 wrapper。

---

## 6. 表单和控制

### 控制类型

- 二元设置使用 switch / checkbox。
- 多选一模式使用 segmented control 或 tabs。
- 数值设置使用 number stepper / slider / input。
- 多条件筛选使用 filter strip，不用把筛选项散落进多个卡片。
- 高风险动作使用 danger button，并在动作前明确对象和影响。

### 交互状态

每个可交互控件至少覆盖：

- default
- hover
- active / selected
- disabled
- loading
- error when relevant
- keyboard focus

### 可访问性

- 使用真实 `button`、`input`、`select`、`nav`、`section` 等语义元素。
- 自定义 tabs、menus、dialogs、toolbars 时遵循 WAI-ARIA APG 的键盘模式。
- 所有可聚焦元素必须有可见 focus 样式。
- 桌面密集按钮建议可点击区域不小于 32px；移动或高频关键操作不小于 40px。
- 图标按钮必须有 `title` 或 `aria-label`。

---

## 7. 状态和反馈

### 数据状态

所有数据视图必须设计：

- loading：正在加载，但保留已知旧数据时要标明。
- empty：没有数据，说明下一步可做什么。
- error：展示错误摘要和重试入口。
- stale：数据过期，明确更新时间。
- partial：部分 provider / 工具失败，但仍可展示可用数据。

### 实时状态

- 自动刷新、手动刷新和后台任务要有明确状态。
- 交易相关 action 在行情 stale 时应禁用或要求重新拉取。
- Agent 后台运行时，页面应显示 run 状态，而不是只在 chat 内出现。

### Motion

- 动效只用于状态过渡、展开收起、刷新中、流式生成。
- 不使用装饰性大动画。
- 尊重 `prefers-reduced-motion`。

---

## 8. 文案

规则：

- 页面文案使用操作语言，不写产品宣传。
- 空状态说明当前状态和下一步，不解释整个系统。
- 错误文案说明失败对象、失败原因和可恢复动作。
- 金融判断要避免保证收益式表达；使用“判断 / 可能 / 风险 / 需复盘”。

示例：

```text
好：行情已过期，请刷新后再让 Agent 生成交易意图。
坏：为了保障您的财富增值，请立即刷新行情。
```

---

## 9. 实现约束

- 新页面先复用现有 shell、tokens、按钮、卡片、时间线和表单模式。
- 不新增外部 UI 组件库。
- 不在组件里散落 inline style，除非是动态宽度、图表尺寸、涨跌条这类数据驱动样式。
- 新增组件命名按领域命名，不写通用但不可复用的 `Box1` / `Card2`。
- 组件内部状态只管理 UI 状态；业务状态通过 hooks / Tauri commands 获取。
- 大列表要注意渲染成本，必要时虚拟化或分页。
- 任何新图表能力优先评估是否应进入 `KLineChart`，而不是页面旁路实现。

---

## 10. 验收标准

- 新页面在 1280px 和 1440px 宽度下没有文字重叠、按钮挤压或无意义横向滚动。
- 所有数字字段格式化一致，空值显示 `-`。
- 所有列表支持必要的排序 / 筛选 / 搜索或明确说明为什么不需要。
- K 线展示统一使用 `KLineChart`。
- Agent 工具调用、账户动作、后台 run 在 UI 中可追溯。
- loading / empty / error / stale / partial 状态都有明确界面。
- 键盘可访问：主导航、tabs、表单、modal、菜单可聚焦和操作。
- 颜色、圆角、间距、字体遵循 `src/styles.css` token，不引入冲突风格。

---

## 11. 参考来源

- Ant Design Data Display：数据展示应按重要性、操作频率和关联度组织，表格适合结构化比较数据。
  https://ant.design/docs/spec/data-display/
- Ant Design Visualization Page：分析页面应 summary first、filters next、details on demand，并控制模块数量。
  https://ant.design/docs/spec/visualization-page/
- W3C WCAG 2.2：focus、target size、可访问性基础约束。
  https://www.w3.org/TR/WCAG22/
- WAI-ARIA Authoring Practices：tabs、menus、dialogs、toolbar 等复杂组件键盘模式。
  https://www.w3.org/WAI/ARIA/apg/
- TradingView Charting Library UI Elements：金融图表常见 toolbar、周期、指标、画线、截图等能力组织。
  https://www.tradingview.com/charting-library-docs/latest/ui_elements/
- TradingView Lightweight Charts Panes：价格、成交量和指标可通过 pane 分离展示。
  https://tradingview.github.io/lightweight-charts/docs/panes
