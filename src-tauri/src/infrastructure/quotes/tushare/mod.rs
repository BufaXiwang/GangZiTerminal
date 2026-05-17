#![allow(dead_code, unused_imports)] // TuShare 接口完整面：calendar / fund_klines / index_klines 等供 Step B agent 工具使用

//! TuShare HTTP adapter——核心数据源。
//!
//! 模块拆分（按数据领域）：
//! - `client`：HTTP POST + token + 错误转换（公共层）
//! - `stock`：stock_basic / daily / weekly / monthly / daily_basic / adj_factor
//! - `index`：index_daily
//! - `mins`：stk_mins 分钟 K（5000+ 积分门槛）
//! - `concept`：concept / concept_detail（Phase 8）
//! - `flow`：moneyflow / moneyflow_hsgt / hsgt_top10 / margin（Phase 6）
//! - `events`：dividend / suspend_d / namechange / forecast / share_float（Phase 7）
//! - `fund`：fund_basic / fund_nav / fund_portfolio / fund_manager
//! - `calendar`：trade_cal（Phase 9）
//! - `top_list`：每日龙虎榜

pub mod calendar;
pub mod client;
pub mod concept;
pub mod events;
pub mod flow;
pub mod fund;
pub mod index;
pub mod probe;
pub mod stock;
