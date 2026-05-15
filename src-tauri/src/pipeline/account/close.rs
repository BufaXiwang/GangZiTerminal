//! 批量平仓——`reset` 内部用 + 未来风控扫描（risk_scan）批量触发用。
//!
//! 单 position 平仓走 `AccountService::close_position`（带规则校验）。
//! 多 position 一次性平仓（如 reset）建议直接走 `AccountService::reset`——
//! 它内部不写 closed event（直接清空 positions），现金自动从空事件链回到 INITIAL_CASH。
//!
//! 本模块定义批量 close 的**结构 + 函数**，给未来风控扫描复用：
//! 一次扫描发现 N 个仓位触发条件 → 构造 `Vec<CloseRequest>` → 顺序调
//! `AccountService::close_position_unchecked`（已带 Mutex 保护）。

use crate::domain::account::types::{CloseReason, EventSource};
use crate::domain::account::PositionId;
use crate::domain::shared::Yuan;

/// 批量平仓的单条请求。
#[derive(Debug, Clone)]
pub struct CloseRequest {
    pub position_id: PositionId,
    pub exit_price: Yuan,
    pub reason: CloseReason,
    pub source: EventSource,
    pub agent_note_md: String,
}

/// 占位：未来 risk_scan 用——逐个走 AccountService 不需要独立 batch 函数，
/// AccountService 自带 Mutex 保护，串行调用即可。
///
/// 当前先不暴露任何函数——避免无 caller 的死代码警告。
pub fn _placeholder() {}
