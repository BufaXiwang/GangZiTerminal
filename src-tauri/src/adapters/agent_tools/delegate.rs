//! `delegate` 工具——主操盘手 agent 把"重活"（深度研究 / 反方挑刺）派给子 agent。
//!
//! 收益：
//! - 重活的中间 tool_result 不进主 context，主 chat 只多 ~500 字"子 agent 简报"
//! - 反方意见在独立 context 里产出，不被主 agent 的旧观点 anchor
//!
//! 调用形态：
//! ```json
//! { "task": "研究茅台 600519 的近期走势 + 资金面", "agent_type": "researcher" }
//! { "task": "我想开仓茅台 200 股止损 ¥1400 — 帮我找漏洞", "agent_type": "bear_advocate" }
//! ```

use crate::domain::agent::types::{ContextBudget, ToolResultContent};
use crate::pipeline::agent::config::{build_provider_for_channel, read_agent_config};
use crate::pipeline::agent::tools::{err_text, ok_json, Tool, ToolContext, ToolRegistry};
use crate::pipeline::agent::{run_subagent, SubAgentType};
use crate::domain::agent::types::PipelineKind;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use tauri::AppHandle;

pub struct DelegateTool {
    app: AppHandle,
}

impl DelegateTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for DelegateTool {
    fn name(&self) -> &'static str {
        "delegate"
    }

    fn description(&self) -> &'static str {
        "派子 agent 跑重活。返回子 agent 的简明结论（≤500 字），不污染主 context。\
        \nagent_type=researcher：深度研究一只股 / 一个板块 / 一个主题（看 K 线图 + 拉资金 + 查资讯 + 聚合简报）。\
        \nagent_type=bear_advocate：唱反调，专找你提案的弱点（至少 3 条反方论据）。\
        开仓前自检 / Bull-Bear Steelman 工程化时调。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "任务描述。researcher 类例：'研究茅台 600519 的近期走势 + 资金面 + 风险点'；bear_advocate 类例：'我想开仓茅台 200 股目标价 ¥1620 止损 ¥1380 — 找这个提案的漏洞'"
                },
                "agent_type": {
                    "type": "string",
                    "enum": ["researcher", "bear_advocate"],
                    "description": "researcher 调研 / bear_advocate 反方"
                }
            },
            "required": ["task", "agent_type"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let task = match input.get("task").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => s.to_string(),
            _ => return err_text("缺少 task"),
        };
        let agent_type = match input.get("agent_type").and_then(Value::as_str) {
            Some(s) => match SubAgentType::parse(s) {
                Some(t) => t,
                None => {
                    return err_text(format!(
                        "未知 agent_type：{s}（应为 researcher / bear_advocate）"
                    ))
                }
            },
            None => return err_text("缺少 agent_type"),
        };

        // 1. 拿 chat 渠道配置——子 agent 继承父 model
        let cfg = read_agent_config(&self.app);
        let (channel, model) = match cfg.resolve_pipeline(PipelineKind::Chat) {
            Ok((c, m)) => (c.clone(), m.to_string()),
            Err(e) => return err_text(format!("解析 chat channel 失败：{e}")),
        };
        let provider = match build_provider_for_channel(&channel) {
            Ok(p) => p,
            Err(e) => return err_text(format!("build provider 失败：{e}")),
        };

        // 2. 构造一个"sub-only registry"——只含读类工具实例。
        //    DelegateTool 自己不在其中（white-list 已经排除），递归保护双保险。
        let sub_base = build_sub_agent_base_registry(&self.app);

        // 3. 父 budget——子 agent 内部会取 1/3
        let parent_budget = ContextBudget {
            soft_limit_tokens: cfg.agent.context_soft_limit_tokens,
            hard_limit_tokens: cfg.agent.context_hard_limit_tokens,
            compact_keep_last_n: cfg.agent.compact_keep_last_n_turns,
            max_search_calls: cfg.agent.max_search_calls_per_run,
        };

        // 4. 跑子 agent
        match run_subagent(
            provider,
            &sub_base,
            task,
            agent_type,
            &ctx.run_id,
            model,
            &parent_budget,
        )
        .await
        {
            Ok(result) => {
                let payload = json!({
                    "agent_type": agent_type.as_str(),
                    "turns": result.turns,
                    "tools_called": result.tools_called,
                    "report": result.final_text,
                });
                (ok_json(payload), false)
            }
            Err(e) => err_text(format!("子 agent 执行失败：{e}")),
        }
    }
}

/// Sub agent 的 base registry——只含读类工具实例（绝不含写类、绝不含 delegate）。
/// 由 DelegateTool 在 execute 时实例化；run_subagent 内部再按 SubAgentType 白名单过滤。
pub(super) fn build_sub_agent_base_registry(app: &AppHandle) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    // 行情读
    reg.register(Arc::new(super::quotes::GetQuoteTool::new(app.clone())));
    reg.register(Arc::new(super::quotes::GetKlineTool::new(app.clone())));
    reg.register(Arc::new(super::quotes::GetMarketOverviewTool::new(
        app.clone(),
    )));
    // 研究读
    reg.register(Arc::new(super::research::ScanMarketTool::new(app.clone())));
    reg.register(Arc::new(super::research::GetTopListTool::new(app.clone())));
    reg.register(Arc::new(super::research::GetMoneyflowTool::new(app.clone())));
    reg.register(Arc::new(super::research::GetConceptPerformanceTool::new(
        app.clone(),
    )));
    reg.register(Arc::new(super::research::GetCompanyEventsTool::new(
        app.clone(),
    )));
    // 资讯读
    reg.register(Arc::new(super::news::SearchNewsTool::new(app.clone())));
    // 账户读
    reg.register(Arc::new(super::account::GetAccountTool::new(app.clone())));
    reg.register(Arc::new(super::positions::GetPositionTool::new(app.clone())));
    // 视觉读（analyze_chart 是读——renderer 渲图给 LLM 看）
    reg.register(Arc::new(super::visual::AnalyzeChartTool::new(app.clone())));
    reg
}
