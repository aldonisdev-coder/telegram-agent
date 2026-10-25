use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct TokenUsage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
}

#[derive(Debug, Clone)]
pub struct GenerateRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub max_tokens: i64,
    pub temperature: f32,
}

#[derive(Debug, Clone)]
pub struct GenerateResponse {
    pub content: String,
    pub usage: TokenUsage,
}

#[async_trait]
pub trait AiProvider: Send + Sync {
    fn name(&self) -> &'static str;
    async fn generate(
        &self,
        req: GenerateRequest,
    ) -> Result<GenerateResponse, crate::ai::error::AiError>;
}
