//! OpenAI-compatible model backends + deterministic mock explorer.

mod mock;
mod openai;
mod types;

pub use mock::{MockModel, MockScript, MockStep};
pub use openai::{validate_api_endpoint, OpenAiCompatBackend};
pub use types::{
    ChatMessage, FunctionCall, MessageRole, ModelBackend, ModelConfig, ModelError, ModelRequest,
    ModelResponse, ToolSpec, Usage,
};
