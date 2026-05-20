//! LLM 本地工具的具体实现 + 组装入口（chat registry）。
//!
//! 抽象（`Tool` trait / `ToolRegistry` / `ToolContext` / `ok_json` / `err_text`）
//! 在 [`crate::pipeline::agent::tools`]——pipeline 只依赖那层抽象。
//!
//! 各 tool 文件按业务归类：
//! - `quotes`：行情查询
//! - `research`：龙虎榜 / 资金流 / 板块涨幅 / 扫盘 / 公司事件
//! - `news`：资讯检索
//! - `account` / `positions`：账户读 + 写（open / close / scale / adjust_position）
//! - `visual`：analyze_chart / propose_visual_pattern
//! - `delegate`：派 researcher / bear_advocate 子 agent
//! - `compact`：compact_now 主动压缩 context

use std::sync::Arc;
use tauri::AppHandle;

use crate::pipeline::agent::tools::ToolRegistry;

pub mod account;
pub mod compact;
pub mod delegate;
pub mod news;
pub mod positions;
pub mod quotes;
pub mod research;
pub mod visual;

/// Chat pipeline 工具注册表——chat / reflection 共用。
pub fn build_chat_registry(app: &AppHandle) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    // 行情
    reg.register(Arc::new(quotes::GetQuoteTool::new(app.clone())));
    reg.register(Arc::new(quotes::GetKlineTool::new(app.clone())));
    reg.register(Arc::new(quotes::GetMarketOverviewTool::new(app.clone())));
    // 研究
    reg.register(Arc::new(research::ScanMarketTool::new(app.clone())));
    reg.register(Arc::new(research::GetTopListTool::new(app.clone())));
    reg.register(Arc::new(research::GetMoneyflowTool::new(app.clone())));
    reg.register(Arc::new(research::GetConceptPerformanceTool::new(app.clone())));
    reg.register(Arc::new(research::GetCompanyEventsTool::new(app.clone())));
    // 资讯
    reg.register(Arc::new(news::SearchNewsTool::new(app.clone())));
    // 账户读
    reg.register(Arc::new(account::GetAccountTool::new(app.clone())));
    reg.register(Arc::new(positions::GetPositionTool::new(app.clone())));
    reg.register(Arc::new(positions::AddToWatchlistTool::new(app.clone())));
    // 账户写——v4 合并 Expectation 后 4 个写工具：open / close / scale / adjust
    reg.register(Arc::new(account::OpenPositionTool::new(app.clone())));
    reg.register(Arc::new(account::ClosePositionTool::new(app.clone())));
    reg.register(Arc::new(account::ScalePositionTool::new(app.clone())));
    reg.register(Arc::new(account::AdjustPositionTool::new(app.clone())));
    // 视觉
    reg.register(Arc::new(visual::AnalyzeChartTool::new(app.clone())));
    reg.register(Arc::new(visual::ProposeVisualPatternTool::new(app.clone())));
    // Sub agent 派遣
    reg.register(Arc::new(delegate::DelegateTool::new(app.clone())));
    // Context 自管
    reg.register(Arc::new(compact::CompactNowTool::new(app.clone())));
    reg
}
