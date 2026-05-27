//! Account BC pipeline — use cases / 后台任务 / 跨 infra 编排。
//!
//! Spec: docs/design/account-module.md §3 / §4 / §5

pub mod eval;
pub mod fills;
pub mod quote_gateway;
pub mod scheduler;
pub mod service;
pub mod snapshot;

pub use eval::{evaluate_account_triggers, EvalDeps};
pub use quote_gateway::{AccountQuoteGateway, QuotesFacadeGateway};
pub use scheduler::{spawn_account_eval_scheduler, AccountSchedulerHandle};
pub use service::{AccountService, AccountServiceConfig};
