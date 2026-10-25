use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::error::AiError;
use super::provider::{GenerateRequest, GenerateResponse, TokenUsage};

#[derive(Clone)]
pub struct OpenAiProvider {
    base_url: String,
    api_key: String,
    client: reqwest::Client,
}

impl OpenAiProvider {
    pub fn new(base_url: String, api_key: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap();
        Self {
            base_url,
            api_key,
            client,
        }
    }
}

#[derive(Serialize)]
struct OaiMessage {
    role: String,
    content: String,
}

#[derive(Serialize)]
struct OaiRequest {
    model: String,
    messages: Vec<OaiMessage>,
    max_tokens: i64,
    temperature: f32,
}

#[derive(Deserialize)]
struct OaiChoice {
    message: OaiMsg,
}
#[derive(Deserialize)]
struct OaiMsg {
    content: Option<String>,
}
#[derive(Deserialize)]
struct OaiUsage {
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
    total_tokens: Option<i64>,
}
#[derive(Deserialize)]
struct OaiResponse {
    choices: Vec<OaiChoice>,
    usage: Option<OaiUsage>,
}
#[derive(Deserialize)]
struct OaiError {
    error: Option<OaiErrDetail>,
}
#[derive(Deserialize)]
struct OaiErrDetail {
    message: Option<String>,
}

#[async_trait]
impl super::provider::AiProvider for OpenAiProvider {
    fn name(&self) -> &'static str {
        "openai"
    }

    async fn generate(&self, req: GenerateRequest) -> Result<GenerateResponse, AiError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = OaiRequest {
            model: req.model,
            messages: req
                .messages
                .into_iter()
                .map(|m| OaiMessage {
                    role: m.role,
                    content: m.content,
                })
                .collect(),
            max_tokens: req.max_tokens,
            temperature: req.temperature,
        };
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| AiError::Provider(e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| AiError::Provider(e.to_string()))?;
        if !status.is_success() {
            let msg = serde_json::from_str::<OaiError>(&text)
                .ok()
                .and_then(|e| e.error)
                .and_then(|d| d.message)
                .unwrap_or(text);
            return Err(AiError::Provider(format!("HTTP {} - {}", status, msg)));
        }
        let parsed: OaiResponse =
            serde_json::from_str(&text).map_err(|e| AiError::Parse(e.to_string()))?;
        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| AiError::Parse("no choices".into()))?;
        let content = choice.message.content.unwrap_or_default();
        let usage = parsed
            .usage
            .map(|u| TokenUsage {
                prompt_tokens: u.prompt_tokens.unwrap_or(0),
                completion_tokens: u.completion_tokens.unwrap_or(0),
                total_tokens: u.total_tokens.unwrap_or(0),
            })
            .unwrap_or(TokenUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
            });
        Ok(GenerateResponse { content, usage })
    }
}
