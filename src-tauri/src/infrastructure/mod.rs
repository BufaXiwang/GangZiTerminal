//! Infrastructure 层——I/O 适配（HTTP / DB / 进程内 cache）。
//!
//! 按 **bounded context (DDD 子域)** 划分：
//! - `quotes/`   —— 行情数据（TuShare / EM / cache / snapshot）
//! - `news/`     —— 资讯子域（未接入）
//! - `account/`  —— 模拟账户子域（未接入）
//!
//! 不被 domain 层 use——通过 trait / function call 注入。

pub mod account;
pub mod quotes;
