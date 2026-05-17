#![allow(dead_code, unused_imports)] // provider/tool 实现侧完整能力面，部分按需启用

//! Infrastructure `agent`——Agent provider 实现 + 工具实现。
//!
//! - `provider/`：ChatProvider trait 实现（anthropic / openai 三个 wire format + retry 包装）
//! - `tools/`：Tool trait 实现（quotes / account / memory / news / positions）
//!
//! domain 层 `domain::agent::types` 只定义类型；这里负责把外部 HTTP / SQLite 错误
//! map 成 domain 错误，调 quotes / account 等其它 BC 的实现来满足工具能力。

pub mod provider;
pub mod repository;
pub mod tools;
