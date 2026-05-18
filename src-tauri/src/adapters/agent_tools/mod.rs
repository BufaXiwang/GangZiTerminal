//! LLM 本地工具的具体实现 + 组装入口（chat registry）。
//!
//! 抽象（`Tool` trait / `ToolRegistry` / `ToolContext` / `ok_json` / `err_text`）
//! 在 [`crate::pipeline::agent::tools`]——pipeline 只依赖那层抽象，不知道这里的具体
//! tool 是谁。本层做"LLM tool 协议 ↔ 领域 use case"的反腐层（anti-corruption）。
//!
//! 各 tool 文件按业务归类：
//! - `quotes`：行情查询
//! - `research`：龙虎榜 / 资金流 / 板块涨幅 / 扫盘 / 公司事件
//! - `news`：资讯检索
//! - `account` / `positions`：账户读 + 写（open / close / scale / adjust_stops）
//! - `theses`：投资论点 create / update_state / attach_feedback
//! - `principles`：投资原则 propose / confirm / retire（v2 重构替代旧 memory tools）

use std::sync::Arc;
use tauri::AppHandle;

use crate::pipeline::agent::tools::ToolRegistry;

pub mod account;
pub mod expectations;
pub mod news;
pub mod positions;
pub mod principles;
pub mod quotes;
pub mod research;
pub mod theses;

/// Chat pipeline 工具注册表——chat / reflection 共用。
///
/// 含：
/// - quotes 系（get_quote / get_kline / get_market_overview）只读
/// - research（scan_market / get_top_list / get_moneyflow / get_concept_performance /
///   get_company_events）只读
/// - news（search_news）只读
/// - account 读（get_account / get_position）+ 写（open / close / scale / adjust_stops）
/// - theses（create_thesis / update_thesis_state / attach_thesis_feedback）写
/// - principles（propose_principle / confirm_principle / retire_principle）写
pub fn build_chat_registry(app: &AppHandle) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    // 行情
    reg.register(Arc::new(quotes::GetQuoteTool::new(app.clone())));
    reg.register(Arc::new(quotes::GetKlineTool::new(app.clone())));
    reg.register(Arc::new(quotes::GetMarketOverviewTool::new(app.clone())));
    // 研究——龙虎榜 / 资金流 / 板块涨幅 / 扫盘 / 公司事件
    reg.register(Arc::new(research::ScanMarketTool::new(app.clone())));
    reg.register(Arc::new(research::GetTopListTool::new(app.clone())));
    reg.register(Arc::new(research::GetMoneyflowTool::new(app.clone())));
    reg.register(Arc::new(research::GetConceptPerformanceTool::new(
        app.clone(),
    )));
    reg.register(Arc::new(research::GetCompanyEventsTool::new(app.clone())));
    // 资讯
    reg.register(Arc::new(news::SearchNewsTool::new(app.clone())));
    // 账户读
    reg.register(Arc::new(account::GetAccountTool::new(app.clone())));
    reg.register(Arc::new(positions::GetPositionTool::new(app.clone())));
    // 账户写——chat 中 agent mid-loop 直接下单
    reg.register(Arc::new(account::OpenPositionTool::new(app.clone())));
    reg.register(Arc::new(account::ClosePositionTool::new(app.clone())));
    reg.register(Arc::new(account::ScalePositionTool::new(app.clone())));
    reg.register(Arc::new(account::AdjustStopsTool::new(app.clone())));
    // Thesis 写——agent 显式管理投资论点
    reg.register(Arc::new(theses::CreateThesisTool::new(app.clone())));
    reg.register(Arc::new(theses::UpdateThesisStateTool::new(app.clone())));
    reg.register(Arc::new(theses::AttachThesisFeedbackTool::new(app.clone())));
    // Principle 写——结构化投资原则 / 已知偏差 / 风险偏好（v2 残留，W23 末删）
    reg.register(Arc::new(principles::ProposePrincipleTool::new(app.clone())));
    reg.register(Arc::new(principles::ConfirmPrincipleTool::new(app.clone())));
    reg.register(Arc::new(principles::RetirePrincipleTool::new(app.clone())));
    // Expectation 写（v3 核心实体）
    reg.register(Arc::new(expectations::CreateExpectationTool::new(app.clone())));
    reg.register(Arc::new(expectations::UpdateExpectationTool::new(app.clone())));
    reg.register(Arc::new(expectations::CancelExpectationTool::new(app.clone())));
    reg
}
