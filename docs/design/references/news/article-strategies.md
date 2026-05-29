# NewsNow 渠道正文抽取策略

> 本文档是 News 模块「按渠道分策略抽取正文」的接入契约。模块级契约见 [../../news-module.md](../../news-module.md)；通用抽取见 [article-extractor.md](article-extractor.md)。

## 背景

NewsNow API（`https://newsnow.busiyi.world/api/s?id=<channel>&latest`，需带浏览器 UA + `Origin: https://newsnow.busiyi.world`）**只返回 `title` / `url` / `pubDate` / 薄 `extra`，不含正文**。正文必须从 `item.url` 指向的源**二次抓取**，且每个渠道结构不同 → 用**策略模式**按 source 分派。

正文在**资讯刷新时同步抓取**（不靠用户点开 drawer 按需抓）；抓不到则不展示并记 log。新增渠道必须在此登记策略 + 在 `infrastructure/news/article/strategy.rs` 实现/映射。

## 策略类型

| 策略 | 含义 |
|---|---|
| `TitleIsContent` | 快讯，标题即全文，不发 HTTP |
| `JsonApi` | 打源的内容 JSON API 取结构化正文 |
| `NextData` | SSR 页面内 `<script id="__NEXT_DATA__">` JSON 内嵌正文 |
| `InitialState` | SSR 页面 `window.initialState` JSON（或 `<meta name=description>` 快捷） |
| `StaticHtml` | 静态页 CSS selector 抽取（注意字符集） |
| `InlineScript` | 正文藏在内联 JS 变量（DOM 容器是空壳，由 JS 填充） |
| `Generic` | 未登记源兜底（readability） |

## 渠道配方（实测 2026-05-29）

| channel | 显示名 | 策略 | 正文来源 | 字符集 |
|---|---|---|---|---|
| `cls-telegraph` | 财联社电报 | NextData | `www.cls.cn/detail/<id>` → `__NEXT_DATA__` | utf-8 |
| `cls-depth` | 财联社深度 | NextData | 同上（共用 detail 页） | utf-8 |
| `wallstreetcn-quick` | 华尔街见闻快讯 | JsonApi | `api-one.wallstcn.com/apiv1/content/lives/<id>` | utf-8(JSON) |
| `wallstreetcn` | 华尔街见闻 | JsonApi | 按 url 判 lives/articles（见下） | utf-8(JSON) |
| `36kr-quick` | 36氪快讯 | InitialState | `www.36kr.com/newsflashes/<id>` → `<meta name=description>` | utf-8 |
| `gelonghui` | 格隆汇 | StaticHtml | `www.gelonghui.com/news/<id>` selector `article.main-news.article-with-html` | utf-8 |
| `fastbull-news` | 法布财经 | StaticHtml | `item.url`（`fastbull.com/cn/news-detail/<id>_1`）selector `.news-detail-content` | utf-8 |
| `cankaoxiaoxi` | 参考消息 | InlineScript | `item.url` 内联 JS `var contentTxt="…"` | utf-8 |
| `sputniknewscn` | 卫星通讯社 | StaticHtml | `item.url`（`sputniknews.cn/YYYYMMDD/<id>.html`）selector `.article__body` | utf-8 |
| `zaobao` | 联合早报(zaochenbao) | StaticHtml | `item.url` selector `#article-body` | **GBK/gb18030** |
| `jin10` | 金十数据 | TitleIsContent | 标题即全文，不抓 | — |

## 具体接入

### CLS（cls-telegraph / cls-depth）— 共用一个抽取器
- 从 item.url 取 `<id>`（形如 `www.cls.cn/detail/<id>`）。请求 `GET https://www.cls.cn/detail/<id>`，仅需 UA。
- 解析 `<script id="__NEXT_DATA__" type="application/json">` 内 JSON。
- 字段：`props.pageProps.articleDetail.content`（正文 HTML，serde 解 `\u`，再 strip tags），`…brief`（摘要），`…title`。
- **不要**走官方 `/v1/article/detail`、`/v1/roll/*`（需 HMAC 签名，errno 10012/50101）。SSR 页 `__NEXT_DATA__` 已含完整正文。telegraph 的 content 比 title 更全，不当 TitleIsContent。

### wallstreetcn-quick
- 从 item.url `…/livenews/<id>` 取数字 id。请求 `GET https://api-one.wallstcn.com/apiv1/content/lives/<id>`，无需特殊 header。
- 字段：`data.content_text`（纯文本，首选）/ `data.content`（HTML）。
- 注意：`wallstreetcn.com/livenews/<id>` 网页本身 404，必须走 api-one。

### wallstreetcn（主站，长文 + 快讯混下发）
- NewsNow `wallstreetcn` 渠道会同时下发 livenews（快讯）和 articles（长文），用 item.url 区分：
  - url 含 `/articles/<id>` → `GET https://api-one.wallstcn.com/apiv1/content/articles/<id>?extract=0` → `data.content`（HTML，**无 content_text**，需 strip tags）。
  - 否则按 livenews 处理 → `…/content/lives/<id>` → `data.content_text`。
- 与 `wallstreetcn-quick` 共用同一抽取器（`WallstreetcnApi`），仅 endpoint 按 url 切换。

### fastbull-news（法布财经）
- 请求 item.url（`www.fastbull.com/cn/news-detail/<id>_1`，utf-8，需浏览器 UA）。selector `.news-detail-content`。
- 无公开 JSON API；静态页正文已完整。

### cankaoxiaoxi（参考消息）
- 请求 item.url（`ckxxapp.ckxx.net/pages/YYYY/MM/DD/<id>.html`，utf-8）。
- 正文**不在 DOM**（`#articleContent` 空壳由 JS 填充），而在内联脚本变量 `var contentTxt = "…";`。
- 取该字符串字面量（找未转义收尾 `"`）→ JS 反转义（`\/`→`/`、`\"`→`"`、`\n`/`\t`）→ strip tags。

### sputniknewscn（卫星通讯社）
- 请求 item.url（`sputniknews.cn/YYYYMMDD/<id>.html`，utf-8）。selector `.article__body`（正文容器，内含多个 `.article__text` 段落）。

### 36kr-quick
- 从 item.url `…/newsflashes/<id>` 取数字 id。请求 `GET https://www.36kr.com/newsflashes/<id>`。
- 取 `<meta name="description">`（== 正文 widgetContent，最省）；或 `window.initialState` JSON → `newsflashDetail.detailData.data.widgetContent`。
- gateway POST API 需签名，返回空，不要用。

### gelonghui
- 请求 `GET https://www.gelonghui.com/news/<id>`（utf-8）。selector `section.article-details article.main-news.article-with-html`。
- 无公开 JSON API（/api/article 404）。备选 `window.__NUXT__` data[0].fetch[*].content。

### zaobao（zaochenbao.com）
- 请求 item.url（NewsNow 直接给完整 url）。**字符集 GBK(gb18030) 必须显式 decode**。selector `article#article-body`，标题 `h1.article-title`。Cloudflare 前置，普通 UA 可过。

### jin10
- `TitleIsContent`：item.title 即完整快讯，不二次抓取。（富文本备选：`flash-api.jin10.com/get_flash_list` 需 `x-app-id` header。）

## 失败处理

- 抓不到正文（网络失败 / 结构变更 / 字段缺失）→ 不写 `ArticleContent.content`、不在 UI 展示正文区，记 `tracing::debug/info`（target `news.article.<strategy>`）便于排查；不阻塞 item 入库、不视为整源失败。
- 策略命中但内容过短（< 阈值）按抽取失败处理（同 article-extractor 的 `too_short`）。
