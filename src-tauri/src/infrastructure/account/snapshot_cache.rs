//! 当前 `AccountSnapshot` in-memory 缓存——前端 / agent / IPC 读取的真源。
//!
//! 与 `infrastructure::quotes::snapshot::market_snapshot` 平级，不持久化到 DB（仅内存）。
//!
//! **更新触发**（3 个）：
//! 1. 写操作完成 —— `AccountService::open/close/scale/adjust/reset` 内部立即 put
//! 2. quotes refresh 完成 —— scheduler 监听 `market-quotes-refreshed` event 触发 put
//! 3. 兜底定时 —— scheduler 盘中 10s / 盘外 60s 强制 refresh
//!
//! **读 API**：`get()` 同步返回当前快照（无则 None，启动初期一两秒内可能未填充）。
//!
//! 写后 emit `account-snapshot-updated` 让前端 hook 重新 invoke `get_account_snapshot` IPC。

use crate::domain::account::types::AccountSnapshot;
use std::sync::{OnceLock, RwLock};

static SNAPSHOT: OnceLock<RwLock<Option<AccountSnapshot>>> = OnceLock::new();

fn store() -> &'static RwLock<Option<AccountSnapshot>> {
    SNAPSHOT.get_or_init(|| RwLock::new(None))
}

/// 同步读——前端 IPC / agent tool 都从这里拿。
pub fn get() -> Option<AccountSnapshot> {
    store().read().ok().and_then(|g| g.clone())
}

/// 写入新快照——AccountService 写后调 / loop 周期性调。
pub fn put(snapshot: AccountSnapshot) {
    if let Ok(mut g) = store().write() {
        *g = Some(snapshot);
    }
}

/// 清空——目前未在生产路径使用；保留给测试 / reset 后兜底。
#[allow(dead_code)]
pub fn clear() {
    if let Ok(mut g) = store().write() {
        *g = None;
    }
}
