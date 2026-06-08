//! Account BC infrastructure — DB schema + repository。
//!
//! Spec: docs/design/account-module.md §2 / §3（依赖约束：可依赖 DB；不依赖 pipeline / adapters）

pub mod migrations;
pub mod repository;

pub use migrations::{migrations, migrations_tail};
pub use repository::AccountRepository;
