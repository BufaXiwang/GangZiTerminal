//! 子 Agent 任务注册表 + 生命周期（spec §3.5 `SubAgentTask`）。
//!
//! Spec: docs/design/agent-infra-module.md §3.5 子 Agent 任务管理
//!
//! `spawn`（register→running）→ `update_progress`（运行中累计 token/tool_uses）→
//! 终态 `finish`（complete/fail/kill，killed 不被覆盖）。`stop_subagent` 经 `mark_killed`
//! 触发 abort 句柄；`subagent_output` 读 `snapshot`。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::task::AbortHandle;

// ───────────────────────── 子 Agent 任务注册表 ─────────────────────────

/// 子 run 状态（spec §3.5 `SubAgentTask.status`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubAgentStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Killed,
}

impl SubAgentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SubAgentStatus::Queued => "queued",
            SubAgentStatus::Running => "running",
            SubAgentStatus::Completed => "completed",
            SubAgentStatus::Failed => "failed",
            SubAgentStatus::Killed => "killed",
        }
    }
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            SubAgentStatus::Completed | SubAgentStatus::Failed | SubAgentStatus::Killed
        )
    }
}

/// 子 run 累计进度（spec §3.5 `SubAgentTask.progress`，从子 run 的 turn / usage 累计）。
#[derive(Debug, Clone, Copy, Default)]
pub struct SubAgentProgress {
    pub tokens: u32,
    pub tool_uses: u32,
    pub duration_ms: u64,
}

/// 一条子 agent 任务（spec §3.5 `SubAgentTask`）。
pub struct SubAgentTask {
    pub agent_id: String,
    pub parent_run_id: String,
    pub conversation_id: String,
    pub description: String,
    pub status: SubAgentStatus,
    pub progress: SubAgentProgress,
    /// 子 run 已产出的最终结果文本（终态时填）。后台任务的 `subagent_output` 读它。
    pub result: Option<String>,
    /// abort 句柄（后台任务才有；前台同步跑不需要）。`stop_subagent` 用它取消。
    pub abort: Option<AbortHandle>,
    /// 防重复完成通知（spec §3.5 `notified`）。
    pub notified: bool,
}

/// 子 agent 任务注册表。主 Agent 通过它管理所有 spawn 出来的子 run（spec §3.5）。
#[derive(Clone, Default)]
pub struct SubAgentTaskRegistry {
    tasks: Arc<Mutex<HashMap<String, SubAgentTask>>>,
}

impl SubAgentTaskRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// spawn：注册一条新任务（初始 `running`）。
    pub(crate) fn register(
        &self,
        agent_id: &str,
        parent_run_id: &str,
        conversation_id: &str,
        description: &str,
        abort: Option<AbortHandle>,
    ) {
        let mut g = self.tasks.lock().expect("SubAgentTaskRegistry poisoned");
        g.insert(
            agent_id.to_string(),
            SubAgentTask {
                agent_id: agent_id.to_string(),
                parent_run_id: parent_run_id.to_string(),
                conversation_id: conversation_id.to_string(),
                description: description.to_string(),
                status: SubAgentStatus::Running,
                progress: SubAgentProgress::default(),
                result: None,
                abort,
                notified: false,
            },
        );
    }

    /// 终态：complete / fail / kill，落进度 + 结果。
    pub(crate) fn finish(
        &self,
        agent_id: &str,
        status: SubAgentStatus,
        progress: SubAgentProgress,
        result: Option<String>,
    ) {
        let mut g = self.tasks.lock().expect("SubAgentTaskRegistry poisoned");
        if let Some(t) = g.get_mut(agent_id) {
            // killed 是终态，不被后续 complete/fail 覆盖。
            if t.status == SubAgentStatus::Killed {
                return;
            }
            t.status = status;
            t.progress = progress;
            t.result = result;
        }
    }

    /// 给一条已注册的任务补挂 abort 句柄（后台任务：先 register 再 spawn 再挂句柄，避免子 run
    /// 瞬时完成时回调早于 register 的竞态）。
    pub(crate) fn set_abort(&self, agent_id: &str, abort: AbortHandle) {
        let mut g = self.tasks.lock().expect("SubAgentTaskRegistry poisoned");
        if let Some(t) = g.get_mut(agent_id) {
            // 若任务已终态（瞬时完成），不再挂句柄（abort 无意义）。
            if !t.status.is_terminal() {
                t.abort = Some(abort);
            }
        }
    }

    /// 运行中增量更新进度（spec §3.5 `update_progress`：按子 run 的 turn 累计 token / tool_uses）。
    /// 终态任务不再更新。
    pub(crate) fn update_progress(&self, agent_id: &str, progress: SubAgentProgress) {
        let mut g = self.tasks.lock().expect("SubAgentTaskRegistry poisoned");
        if let Some(t) = g.get_mut(agent_id) {
            if !t.status.is_terminal() {
                t.progress = progress;
            }
        }
    }

    /// 回写子 run 的真实 conversation_id（`run_forked_agent` 内生成后回填，审计可从注册表定位
    /// 子 run 的 agent_messages 做 replay）。
    pub(crate) fn set_conversation_id(&self, agent_id: &str, conversation_id: &str) {
        let mut g = self.tasks.lock().expect("SubAgentTaskRegistry poisoned");
        if let Some(t) = g.get_mut(agent_id) {
            t.conversation_id = conversation_id.to_string();
        }
    }

    /// 标记一条任务为 killed（abort 句柄触发后），返回是否成功（任务存在且非终态）。
    pub fn mark_killed(&self, agent_id: &str) -> bool {
        let mut g = self.tasks.lock().expect("SubAgentTaskRegistry poisoned");
        match g.get_mut(agent_id) {
            Some(t) if !t.status.is_terminal() => {
                if let Some(h) = &t.abort {
                    h.abort();
                }
                t.status = SubAgentStatus::Killed;
                true
            }
            _ => false,
        }
    }

    /// 读一条任务的快照（status / progress / result），用于 `subagent_output`。
    pub fn snapshot(&self, agent_id: &str) -> Option<(SubAgentStatus, SubAgentProgress, Option<String>)> {
        let g = self.tasks.lock().expect("SubAgentTaskRegistry poisoned");
        g.get(agent_id)
            .map(|t| (t.status, t.progress, t.result.clone()))
    }

    /// 取并置位 `notified`（防重复通知）：返回 true 表示本次是第一次通知。
    pub(crate) fn take_notify_flag(&self, agent_id: &str) -> bool {
        let mut g = self.tasks.lock().expect("SubAgentTaskRegistry poisoned");
        match g.get_mut(agent_id) {
            Some(t) if !t.notified => {
                t.notified = true;
                true
            }
            _ => false,
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.tasks.lock().unwrap().len()
    }

    #[cfg(test)]
    pub fn status_of(&self, agent_id: &str) -> Option<SubAgentStatus> {
        self.tasks.lock().unwrap().get(agent_id).map(|t| t.status)
    }
}

