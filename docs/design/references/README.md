# Design References

> 本目录保存 provider / channel adapter 的参考契约。模块级领域契约仍以 `docs/design/*-module.md` 为准。

## 定位

Reference 文档回答：

```text
这个外部渠道怎么获取数据？
原始字段如何映射到模块 canonical model？
单位、代码、时间如何 normalize？
失败、限流、超时、缺字段如何处理？
这个 adapter 的验收标准是什么？
```

Reference 不回答：

```text
模块对外 command / query 长什么样？
跨模块事件如何路由？
Agent 如何做投资判断？
Account 如何执行交易规则？
```

这些分别写在模块 spec 和 `orchestration.md`。

## 目录

```text
references/
  quotes/
    tdx.md
    eastmoney.md
    tushare.md
    tencent.md
    sina.md
  news/
    rss.md
    newsnow.md
    article-extractor.md
  agent/
    anthropic-messages.md
    openai-responses.md
    openai-chat-completions.md
```

## 规则

- 新增 provider / channel 前先新增 reference。
- 模块 spec 只引用 provider 选择策略和 canonical contract。
- Reference 只约束 adapter，不定义模块对外 API。
- 非官方 endpoint 易变；reference 可以写获取方式和字段语义，但不要把具体 URL 当成跨模块强契约。
