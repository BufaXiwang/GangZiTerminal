//! Eastmoney quote / kline adapter — TDX fallback + BJ 主源。
//!
//! Spec: docs/design/quotes-module.md §5；docs/design/references/quotes/eastmoney.md

pub mod client;

pub use client::EastmoneyProvider;
