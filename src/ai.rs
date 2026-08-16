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
    /// Some OpenAI-compatible providers send `"content": null` (e.g. a tool
    /// call with no text). `sanitize` already treats an empty string as "no
    /// comment", so `None` degrades cleanly instead of failing the parse.
    #[serde(default)]
    content: Option<String>,
}

fn endpoint(base_url: &str) -> String {
    format!("{}/chat/completions", base_url.trim_end_matches('/'))
}

/// Cap on upstream error-body text embedded in log lines.
///
/// Bounds two things: an unbounded dump when a misconfigured `AI_BASE_URL`
/// returns an HTML error page, and — the more serious case — a proxy or
/// self-hosted gateway that echoes the request (including the `Authorization`
/// header) back in a 4xx body, which would otherwise put `AI_API_KEY` in the
/// log via `comment.rs`'s `warn!`.
const MAX_LOGGED_ERROR_BODY_CHARS: usize = 200;

/// Truncate by `char`, not byte: a byte-index cut can land mid-codepoint on
/// multi-byte UTF-8 (e.g. CJK) and panic.
fn truncate_for_log(s: &str) -> String {
    if s.chars().count() <= MAX_LOGGED_ERROR_BODY_CHARS {
        return s.to_string();
    }
    let mut out: String = s.chars().take(MAX_LOGGED_ERROR_BODY_CHARS).collect();
    out.push('…');
    out
}

fn first_choice(resp: ChatResponse) -> Result<String> {
    resp.choices
        .into_iter()
        .next()
        .map(|c| c.message.content.unwrap_or_default())
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
            anyhow::bail!("AI API error ({}): {}", status, truncate_for_log(&text));
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

    #[test]
    fn test_response_with_null_content_treated_as_empty() {
        // Several OpenAI-compatible providers return `"content": null` (e.g.
        // when the model calls a tool or emits nothing). It must degrade to
        // an empty string rather than failing the whole parse and dropping
        // the comment.
        let body = r#"{
            "choices": [
                {"message": {"role": "assistant", "content": null}}
            ]
        }"#;
        let parsed: ChatResponse = serde_json::from_str(body).unwrap();
        assert_eq!(first_choice(parsed).unwrap(), "");
    }

    #[test]
    fn test_truncate_for_log_caps_at_200_chars_by_char_not_byte() {
        // Multi-byte characters must not panic on a byte-index truncation.
        let s: String = "日".repeat(300);
        let out = truncate_for_log(&s);
        assert_eq!(out.chars().count(), 201); // 200 chars + ellipsis
        assert!(out.ends_with('…'));
    }

    #[test]
    fn test_truncate_for_log_passthrough_when_short() {
        assert_eq!(truncate_for_log("short body"), "short body");
    }
}
