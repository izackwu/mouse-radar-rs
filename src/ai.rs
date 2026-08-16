use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A one-shot text completion service.
///
/// Exists as a trait so tests can substitute a stub, matching the
/// `strava::StravaApi` pattern.
#[async_trait]
pub trait AiClient: Send + Sync {
    async fn comment(&self, system: &str, user: &str) -> Result<String>;
}

// --- OpenAI-compatible wire format ---

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    max_tokens: u32,
    temperature: f32,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatResponseMessage,
}

#[derive(Deserialize)]
struct ChatResponseMessage {
    content: String,
}

fn endpoint(base_url: &str) -> String {
    format!("{}/chat/completions", base_url.trim_end_matches('/'))
}

fn first_choice(resp: ChatResponse) -> Result<String> {
    resp.choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .ok_or_else(|| anyhow::anyhow!("AI response contained no choices"))
}

// --- Real implementation ---

/// Client for any endpoint speaking the `OpenAI` chat-completions format.
///
/// Provider is selected entirely by `base_url` and `model` — no code change
/// is needed to move between `OpenAI`, Groq, `DeepSeek`, `OpenRouter`, Ollama, or
/// Anthropic's compatibility endpoint.
pub struct OpenAiCompatClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    api_key: String,
}

impl OpenAiCompatClient {
    pub fn new(
        base_url: String,
        model: String,
        api_key: String,
        timeout: Duration,
    ) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder().timeout(timeout).build()?,
            base_url,
            model,
            api_key,
        })
    }
}

#[async_trait]
impl AiClient for OpenAiCompatClient {
    async fn comment(&self, system: &str, user: &str) -> Result<String> {
        let body = ChatRequest {
            model: &self.model,
            messages: vec![
                ChatMessage {
                    role: "system",
                    content: system,
                },
                ChatMessage {
                    role: "user",
                    content: user,
                },
            ],
            max_tokens: 150,
            temperature: 0.8,
        };

        let resp = self
            .http
            .post(endpoint(&self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await?;

        // Never include the request body or headers in the error: the
        // Authorization header carries the API key.
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("AI API error ({}): {}", status, text);
        }

        first_choice(resp.json().await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_endpoint_appends_path() {
        assert_eq!(
            endpoint("https://api.openai.com/v1"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn test_endpoint_strips_trailing_slash() {
        assert_eq!(
            endpoint("http://localhost:11434/v1/"),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn test_request_serializes_to_openai_shape() {
        let req = ChatRequest {
            model: "gpt-4o-mini",
            messages: vec![
                ChatMessage {
                    role: "system",
                    content: "be brief",
                },
                ChatMessage {
                    role: "user",
                    content: "hello",
                },
            ],
            max_tokens: 150,
            temperature: 0.8,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["model"], "gpt-4o-mini");
        assert_eq!(json["messages"][0]["role"], "system");
        assert_eq!(json["messages"][0]["content"], "be brief");
        assert_eq!(json["messages"][1]["role"], "user");
        assert_eq!(json["max_tokens"], 150);
    }

    #[test]
    fn test_response_deserializes_first_choice() {
        let body = r#"{
            "choices": [
                {"message": {"role": "assistant", "content": "Nice run."}},
                {"message": {"role": "assistant", "content": "ignored"}}
            ]
        }"#;
        let parsed: ChatResponse = serde_json::from_str(body).unwrap();
        assert_eq!(first_choice(parsed).unwrap(), "Nice run.");
    }

    #[test]
    fn test_response_with_no_choices_errors() {
        let parsed: ChatResponse = serde_json::from_str(r#"{"choices": []}"#).unwrap();
        assert!(first_choice(parsed).is_err());
    }
}
