//! Provider 抽象层。
//!
//! 一个 Provider 知道怎么把 [`AgentRequest`] 翻译成自己的 wire format，
//! 调一次远端，把响应流式翻译回 [`ProviderEvent`]。**Provider 不管 tool-use
//! 循环、不管 context compaction、不管观测**——这些是 loop 层的事。
//!
//! 设计上让 Provider 只暴露最小接口：能力声明 + 一次流式调用。
//! 加新协议（OpenAI / DeepSeek / Doubao / 火山）就是新加一个文件实现 [`ChatProvider`]。

pub mod anthropic;
pub mod models;
pub mod openai;
pub mod retry;

use crate::domain::agent::types::{AgentRequest, Message, StopReason};
use async_trait::async_trait;
use futures_util::stream::BoxStream;
use thiserror::Error;

/// 一个 provider 的运行时实例（base_url + token + http client 都在里面）。
/// 复用同一个实例可以复用 reqwest 连接池。
#[async_trait]
pub trait ChatProvider: Send + Sync {
    /// 启动一次流式调用。返回的 stream 在远端关闭或出错时结束。
    /// 调用方（loop）必须 drain 完这个 stream，最后一个事件总是 [`ProviderEvent::MessageComplete`]
    /// 或一个错误。
    async fn stream(
        &self,
        req: &AgentRequest,
    ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError>;
}

/// Provider 流式输出的事件——粒度比 SSE 原始事件粗，比最终消息细。
///
/// 4 种事件覆盖了所有 provider 的需求：
/// - 文本/思考增量是给 UI 实时渲染用
/// - Usage 给观测落表
/// - MessageComplete 给 loop 决定下一轮动作
///
/// **不暴露 tool_use 流式增量**——tool_use 的 input JSON 是流式拼接出来的，但
/// 半成品对 UI 没价值（输入小，等齐了再 emit ToolStart 体验更稳）。Provider
/// 内部把 SSE 拼齐后整块塞进 MessageComplete.message。
#[derive(Debug, Clone)]
pub enum ProviderEvent {
    /// assistant 文本片段。可以多次出现（一个回合内可能多个 text block 之间夹 tool_use）。
    TextDelta(String),
    /// thinking 块的增量。仅 Anthropic / DeepSeek-R1 有。
    ThinkingDelta(String),
    /// 这一回合的 token 用量。Anthropic 在 message_delta 里给最终值，
    /// 我们只在收到时 emit 一次。
    Usage(TokenUsage),
    /// 整条 assistant 消息已经组装完成 + stop_reason 已知。loop 收到后：
    /// - 把 message append 进 messages 列表
    /// - 看 stop_reason：tool_use → 执行工具 → 下一轮；end_turn → 结束 run
    MessageComplete {
        message: Message,
        stop_reason: StopReason,
    },
}

#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_write_tokens: u32,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    /// 4xx——请求构造问题（鉴权失败、参数错、超 context）。不重试。
    #[error("provider rejected request ({status}): {body}")]
    Request { status: u16, body: String },
    /// 429 限速——loop 可以选择退避重试。
    #[error("provider rate limited: {0}")]
    RateLimited(String),
    /// 5xx / 网络层错误——可重试。
    #[error("provider transient error: {0}")]
    Transient(String),
    /// SSE 流解析失败、协议形态不符——不重试，要修代码。
    #[error("provider protocol error: {0}")]
    Protocol(String),
    /// 客户端配置问题（缺 token / base_url 不合法等）。
    #[error("provider config error: {0}")]
    Config(String),
}
