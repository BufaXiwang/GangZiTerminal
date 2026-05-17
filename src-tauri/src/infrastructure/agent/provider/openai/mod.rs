//! OpenAI 兼容 provider——支持两个 wire format：
//! - Responses API (`/v1/responses`)：OpenAI 当前主推的 agent 形态，built-in tools
//!   + 更高 cache 利用率。GPT-5.5 / o3 系列推荐走这条。
//! - Chat Completions (`/v1/chat/completions`)：经典形态，DeepSeek / 火山方舟 /
//!   vLLM / Ollama 等"OpenAI 兼容"端点都仿这个。
//!
//! 抽象轴是 wire format 而不是厂商——未来加 DeepSeek 不需要新 provider，只要
//! 在 ChatCompletionsProvider 上换 base_url 就行。
//!
//! Canonical Block 形态映射：
//!
//! | Block             | Responses API                           | Chat Completions                 |
//! |-------------------|-----------------------------------------|----------------------------------|
//! | Text              | `message.content[input_text/output_text]` | `messages[].content` (string)   |
//! | Image             | `message.content[input_image]`          | `content[image_url]` (data URL) |
//! | ToolUse           | `function_call` (顶层 Item, 带 call_id) | `tool_calls[]` (assistant 消息) |
//! | ToolResult        | `function_call_output` (顶层 Item)      | role=tool 消息                   |
//! | Thinking          | 丢弃（v1）                              | 丢弃                             |
//! | ServerSide(websearch) | 翻译成 `{type:"web_search"}`        | 不支持，丢弃                     |

pub mod chat_completions;
pub mod common;
pub mod responses;

pub use chat_completions::{OpenAIChatCompletionsConfig, OpenAIChatCompletionsProvider};
pub use common::ReasoningEffort;
pub use responses::{OpenAIResponsesConfig, OpenAIResponsesProvider};
