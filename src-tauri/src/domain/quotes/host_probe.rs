//! TDX 行情站点延时探测结果（前端「延时 popup」读模型）。
//!
//! Spec: docs/design/quotes-module.md §TDX 连接池与并发
//!
//! 纯数据 DTO，无 I/O。由 `infrastructure/quotes/tdx` 并行探测填充，
//! 经 `probe_tdx_hosts` command 推给前端。

use serde::{Deserialize, Serialize};
use specta::Type;

/// 单台 TDX HQ 站点的探测结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct HostProbe {
    /// 站点中文名（来自 `HQ_HOSTS`）。
    pub name: String,
    /// IP / 域名。
    pub host: String,
    /// 端口（默认 7709，个别 80）。
    pub port: u16,
    /// connect + handshake 往返延时（毫秒）；连不上 / 握手失败为 `None`。
    pub latency_ms: Option<u32>,
    /// 是否可达（探测成功）。
    pub ok: bool,
    /// 是否被选进当前 active 连接池（低延时子集）。
    pub in_pool: bool,
}
