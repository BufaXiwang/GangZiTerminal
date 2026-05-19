//! Sub Agent——主 agent 通过 `delegate` 工具派出的研究 / 反方 子 agent。
//!
//! 设计：
//! - **复用 `run_agent`**：sub agent 的本质是用受限工具集 + 短 budget + 专用
//!   system prompt 跑一次 [`run_agent`]，最后返回 final assistant text 给主 agent。
//! - **工具白名单**：只读类——绝不允许子 agent 写库 / 开仓 / 调 delegate 本身
//!   （递归保护）。
//! - **独立 context budget**（父的 1/3），独立 messages，不污染主 context。
//! - **失败兜底**：子 agent 报错时返回 [`SubAgentError`]，[`DelegateTool`] 把它
//!   转成 ToolResult is_error；主 agent 看到错误描述继续，不崩。
//!
//! 投资场景下的两种用途：
//! - [`SubAgentType::Researcher`]：深度调研一只股 / 一个板块，返回 ≤500 字简报。
//!   省主 context 大量 tool_result 累积。
//! - [`SubAgentType::BearAdvocate`]：找主 agent 提案的弱点，至少 3 条反对论据。
//!   独立 context = 真正"客观"的反方，治"自我证实偏误"（§ 2 Bull/Bear Steelman
//!   的工程化兜底）。

use crate::domain::agent::types::{
    AgentEvent, AgentOptions, AgentRequest, Block, ContextBudget, Message, PipelineKind, Role,
    SystemBlock,
};
use crate::infrastructure::agent::provider::ChatProvider;
use crate::pipeline::agent::loop_::{run_agent, AgentError};
use crate::pipeline::agent::tools::{ToolContext, ToolRegistry};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc;

const SUB_AGENT_MAX_TURNS: u32 = 5;
const SUB_AGENT_MAX_TOKENS: u32 = 4_096;
const SUB_AGENT_TOOL_TIMEOUT_SECS: u32 = 30;

/// Sub agent 的两种类型——每种自带独立的 system prompt + 工具白名单。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubAgentType {
    /// 研究员：深度调研一只股 / 一个板块 / 一个主题，返回 ≤500 字研究简报。
    Researcher,
    /// 反方律师：找主 agent 提案的弱点，至少 3 条反对论据。
    BearAdvocate,
}

impl SubAgentType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Researcher => "researcher",
            Self::BearAdvocate => "bear_advocate",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "researcher" => Some(Self::Researcher),
            "bear_advocate" => Some(Self::BearAdvocate),
            _ => None,
        }
    }

    /// 这种子 agent 准许调用的工具白名单（不含 delegate 本身——递归保护）。
    pub fn allowed_tools(self) -> &'static [&'static str] {
        match self {
            // 研究员：11 个读类工具——可以做完整的"调研流程"
            Self::Researcher => &[
                "get_quote",
                "get_kline",
                "get_market_overview",
                "search_news",
                "scan_market",
                "get_top_list",
                "get_moneyflow",
                "get_concept_performance",
                "get_company_events",
                "get_position",
                "get_account",
                "analyze_chart",
            ],
            // 反方律师：精选 5 个工具——专心挑刺，工具少不容易跑偏
            Self::BearAdvocate => &[
                "get_quote",
                "get_kline",
                "search_news",
                "get_moneyflow",
                "get_company_events",
            ],
        }
    }

    pub fn system_prompt(self) -> &'static str {
        match self {
            Self::Researcher => RESEARCHER_SYSTEM,
            Self::BearAdvocate => BEAR_ADVOCATE_SYSTEM,
        }
    }
}

const RESEARCHER_SYSTEM: &str = r#"你是 GangZiTerminal 的研究员子 agent，由主操盘手 agent 派出。

**你的任务**：对一个标的 / 板块 / 主题做深度研究，返回一份**简明研究简报（≤500 字）**给主 agent。

**行为契约**：
- **不做交易决策**，不调写库工具——你的工作是收集 + 分析事实
- **≤5 个 tool call**，别陷入无限调研
- get_kline 默认用 mode=chart 模式（一图省 80% token）

**报告结构（按此输出）**：
1. **标的快照**：当前价 / 区间涨跌 / 量能
2. **趋势 / 形态**：看 K 线图判断（突破 / 区间 / 趋势线）
3. **资金 / 资讯背景**：主力动向、近期关键资讯（≤2 句）
4. **关键风险点**：至少 1 个反向风险（学 § 2 Bull/Bear）
5. **结论一句**：值得关注 / 不值得 / 需要更多数据

风格：操盘手语言、直接克制、引数据说话，不写营销话术。
"#;

const BEAR_ADVOCATE_SYSTEM: &str = r#"你是 GangZiTerminal 的**反方律师**子 agent，由主操盘手 agent 派出。

**你的任务**：主 agent 给了你一个交易提案——你的工作是**找它的弱点**，输出至少 **3 条反对论据**。

**行为契约**：
- **专心做反方**——不给"平衡建议"、不给"两面观"、**只输出反方**
- 每条反对论据必须基于**数据 / 资讯证据**，不是泛泛而谈
- **≤4 个 tool call**——挑刺不需要全维度调研，只需要找最致命的 1-3 个漏洞

**输出格式**：
- Bear case 1: <一句话总结> — <数据/资讯证据>
- Bear case 2: ...
- Bear case 3: ...
- **最强 case**: <编号> — <为什么这是最致命的反对，主 agent 必须先反驳这个才能开仓>

不要写"但是从乐观角度..." / "也有可能..."——你是反方，反方就反到底。
你的价值就在于让主 agent 看到自己**没看到的盲点**。
"#;

/// 子 agent run 的最终产出。
#[derive(Debug, Clone)]
pub struct SubAgentResult {
    /// 子 agent 最终 assistant 文本——主 agent 看到的就这个。
    pub final_text: String,
    /// 元信息：跑了几轮、调了几个工具（前端展示 / 复盘用）。
    pub turns: u32,
    pub tools_called: u32,
}

#[derive(Debug, Error)]
pub enum SubAgentError {
    #[error("sub agent run failed: {0}")]
    Run(#[from] AgentError),
    #[error("sub agent returned empty text after {turns} turns")]
    EmptyOutput { turns: u32 },
}

/// 跑一次 sub agent。返回最终文本 + 元信息。
///
/// `provider`：直接复用主 agent 的 provider 实例（继承 model + 渠道）。
/// `parent_registry`：主 registry——本函数会按 `agent_type.allowed_tools()` 过滤
///   出一个子 registry，子 agent 看不到 delegate 本身（递归保护）。
/// `parent_run_id` 用于事件追踪——子 run_id 形如 `{parent}-sub-{type}`。
/// `parent_model`：主 agent 用的模型 id，子 agent 直接继承。
/// `parent_budget`：父 budget，子 agent 用其 1/3。
pub async fn run_subagent(
    provider: Arc<dyn ChatProvider>,
    parent_registry: &ToolRegistry,
    task: String,
    agent_type: SubAgentType,
    parent_run_id: &str,
    parent_model: String,
    parent_budget: &ContextBudget,
) -> Result<SubAgentResult, SubAgentError> {
    // 1. 过滤工具白名单——子 registry 只含该 sub agent 类型许可的工具，
    //    delegate 自动被排除（白名单里没有），实现递归保护
    let sub_registry = Arc::new(filter_registry(parent_registry, agent_type.allowed_tools()));

    // 2. 独立 budget（父的 1/3）——子 agent 不需要主 context 那种规模
    let budget = ContextBudget {
        soft_limit_tokens: parent_budget.soft_limit_tokens / 3,
        hard_limit_tokens: parent_budget.hard_limit_tokens / 3,
        compact_keep_last_n: 2,
        max_search_calls: 0,
    };

    // 3. 构造 AgentRequest——独立 system / messages / options
    let req = AgentRequest {
        system: vec![SystemBlock {
            text: agent_type.system_prompt().to_string(),
            cache_control: false,
        }],
        tools: sub_registry.to_tool_defs(false),
        messages: vec![Message {
            role: Role::User,
            content: vec![Block::Text {
                text: task,
                cache_control: false,
            }],
        }],
        options: AgentOptions {
            model: parent_model,
            max_tokens: SUB_AGENT_MAX_TOKENS,
            temperature: Some(0.5),
            top_p: None,
            // 子 agent 不开 thinking——它的任务是聚合 + 总结，不是深度推理
            thinking: None,
            effort: None,
            max_turns: SUB_AGENT_MAX_TURNS,
            stop_sequences: vec![],
            tool_timeout_secs: Some(SUB_AGENT_TOOL_TIMEOUT_SECS),
        },
        budget,
        trigger_message_id: None,
        pipeline: PipelineKind::Chat,
    };

    // 4. 独立 run_id + tool_ctx
    let sub_run_id = format!("{parent_run_id}-sub-{}", agent_type.as_str());
    let tool_ctx = ToolContext {
        run_id: sub_run_id.clone(),
    };

    // 5. 起内部 event channel——只 drain TextDelta 拼接 final_text；其他事件丢弃
    //    （sub agent 的中间状态不冒泡给主 chat 前端，避免 UI 混乱）
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
    let drain = tokio::spawn(async move {
        let mut final_text = String::new();
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::TextDelta { delta, .. } = ev {
                final_text.push_str(&delta);
            }
        }
        final_text
    });

    // 6. 跑 run_agent——sub agent 不启用 Summarize tier（budget 小 + turns 短不需要）
    let summary = run_agent(provider, None, sub_registry, req, tool_ctx, tx).await?;

    // 7. drain final_text
    let final_text = drain.await.unwrap_or_default();
    if final_text.trim().is_empty() {
        return Err(SubAgentError::EmptyOutput {
            turns: summary.turns,
        });
    }

    Ok(SubAgentResult {
        final_text,
        turns: summary.turns,
        tools_called: summary.local_tool_calls + summary.server_tool_calls,
    })
}

/// 按白名单从父 registry 抽出工具组成新的子 registry。
/// 白名单里有但父 registry 没注册的名字会被静默跳过（让上游用 `tools()` 看到自己注册的能力）。
fn filter_registry(parent: &ToolRegistry, allowed: &[&str]) -> ToolRegistry {
    let mut sub = ToolRegistry::new();
    for name in allowed {
        if let Some(tool) = parent.get(name) {
            sub.register(tool.clone());
        }
    }
    sub
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::types::ToolResultContent;
    use crate::pipeline::agent::tools::Tool;
    use async_trait::async_trait;
    use serde_json::Value;

    struct DummyTool {
        name: &'static str,
    }

    #[async_trait]
    impl Tool for DummyTool {
        fn name(&self) -> &'static str {
            self.name
        }
        fn description(&self) -> &'static str {
            "dummy"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            _input: Value,
            _ctx: &ToolContext,
        ) -> (Vec<ToolResultContent>, bool) {
            (
                vec![ToolResultContent::Text {
                    text: "dummy result".into(),
                }],
                false,
            )
        }
    }

    #[test]
    fn researcher_allowed_tools_excludes_writes() {
        let allowed = SubAgentType::Researcher.allowed_tools();
        // 写类工具一律不在白名单
        for forbidden in [
            "open_position",
            "close_position",
            "scale_position",
            "adjust_stops",
            "create_expectation",
            "update_expectation",
            "cancel_expectation",
            "propose_heuristic",
            "retire_heuristic",
            "propose_visual_pattern",
            "delegate", // 递归保护
        ] {
            assert!(
                !allowed.contains(&forbidden),
                "researcher 白名单不应包含写类工具 {forbidden}"
            );
        }
    }

    #[test]
    fn bear_advocate_allowed_tools_is_minimal() {
        let allowed = SubAgentType::BearAdvocate.allowed_tools();
        // 5 个读类，聚焦挑刺
        assert!(allowed.len() <= 6);
        assert!(allowed.contains(&"get_quote"));
        assert!(allowed.contains(&"get_kline"));
        assert!(!allowed.contains(&"delegate"));
    }

    #[test]
    fn filter_registry_only_keeps_allowed() {
        let mut parent = ToolRegistry::new();
        parent.register(Arc::new(DummyTool { name: "get_quote" }));
        parent.register(Arc::new(DummyTool { name: "open_position" })); // 写类
        parent.register(Arc::new(DummyTool { name: "delegate" }));      // meta

        let sub = filter_registry(&parent, &["get_quote", "get_kline"]);
        // get_quote 在白名单 + 父 registry 有 → 留
        assert!(sub.get("get_quote").is_some());
        // open_position 不在白名单 → 不留
        assert!(sub.get("open_position").is_none());
        // delegate 不在白名单 → 不留（递归保护起效）
        assert!(sub.get("delegate").is_none());
        // get_kline 在白名单但父 registry 没注册 → 静默跳过
        assert!(sub.get("get_kline").is_none());
    }

    #[test]
    fn parse_known_types() {
        assert_eq!(
            SubAgentType::parse("researcher"),
            Some(SubAgentType::Researcher)
        );
        assert_eq!(
            SubAgentType::parse("bear_advocate"),
            Some(SubAgentType::BearAdvocate)
        );
        assert_eq!(SubAgentType::parse("invalid"), None);
    }
}
