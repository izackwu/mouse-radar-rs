use anyhow::Result;
use async_trait::async_trait;
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// What the model returned, plus the diagnostics needed to explain an empty
/// completion.
///
/// `content` alone cannot distinguish "the model had nothing to say" from
/// "the model was cut off before it emitted anything" — a real failure mode
/// with reasoning models, which spend the token budget on hidden reasoning
/// and then return `finish_reason: "length"` with empty content.
#[derive(Debug, Clone, Default)]
pub struct AiResponse {
    pub content: String,
    /// `"stop"`, `"length"`, `"content_filter"`, … Provider-specific.
    pub finish_reason: Option<String>,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    /// Non-visible reasoning tokens, when the provider reports them. A large
    /// value with empty content is the signature of a starved reasoning model.
    pub reasoning_tokens: Option<u32>,
}

impl AiResponse {
    /// One-line diagnostic summary, safe to log and to show an admin.
    #[must_use]
    pub fn diagnostics(&self) -> String {
        let mut parts = vec![format!("chars={}", self.content.chars().count())];
        if let Some(r) = &self.finish_reason {
            parts.push(format!("finish_reason={}", r));
        }
        if let Some(t) = self.prompt_tokens {
            parts.push(format!("prompt_tokens={}", t));
        }
        if let Some(t) = self.completion_tokens {
            parts.push(format!("completion_tokens={}", t));
        }
        if let Some(t) = self.reasoning_tokens {
            parts.push(format!("reasoning_tokens={}", t));
        }
        parts.join(" ")
    }
}

/// A one-shot text completion service.
///
/// Exists as a trait so tests can substitute a stub, matching the
/// `strava::StravaApi` pattern.
#[async_trait]
pub trait AiClient: Send + Sync {
    async fn comment(&self, system: &str, user: &str) -> Result<AiResponse>;
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
    #[serde(default)]
    usage: Option<ChatUsage>,
}

#[derive(Deserialize)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens_details: Option<ChatUsageDetails>,
}

#[derive(Deserialize)]
struct ChatUsageDetails {
    #[serde(default)]
    reasoning_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatResponseMessage,
    #[serde(default)]
    finish_reason: Option<String>,
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

fn first_choice(resp: ChatResponse) -> Result<AiResponse> {
    let (prompt_tokens, completion_tokens, reasoning_tokens) = match resp.usage {
        Some(u) => (
            u.prompt_tokens,
            u.completion_tokens,
            u.completion_tokens_details.and_then(|d| d.reasoning_tokens),
        ),
        None => (None, None, None),
    };

    let choice = resp
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("AI response contained no choices"))?;

    Ok(AiResponse {
        content: choice.message.content.unwrap_or_default(),
        finish_reason: choice.finish_reason,
        prompt_tokens,
        completion_tokens,
        reasoning_tokens,
    })
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
    max_tokens: u32,
}

impl OpenAiCompatClient {
    pub fn new(
        base_url: String,
        model: String,
        api_key: String,
        timeout: Duration,
        max_tokens: u32,
    ) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder().timeout(timeout).build()?,
            base_url,
            model,
            api_key,
            max_tokens,
        })
    }
}

#[async_trait]
impl AiClient for OpenAiCompatClient {
    async fn comment(&self, system: &str, user: &str) -> Result<AiResponse> {
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
            max_tokens: self.max_tokens,
            temperature: 0.8,
        };

        debug!(
            "AI request: model={} max_tokens={} system_chars={} user_chars={}",
            self.model,
            self.max_tokens,
            system.chars().count(),
            user.chars().count()
        );

        let started = std::time::Instant::now();
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

        // Read as text first so a shape we can't deserialize is still
        // reportable — otherwise an unexpected schema fails with a serde
        // message that names a field but never shows the body.
        let raw = resp.text().await?;
        let parsed: ChatResponse = serde_json::from_str(&raw).map_err(|e| {
            anyhow::anyhow!(
                "AI response did not parse ({}): {}",
                e,
                truncate_for_log(&raw)
            )
        })?;

        let out = first_choice(parsed)?;
        let elapsed_ms = started.elapsed().as_millis();

        if out.content.trim().is_empty() {
            // The failure mode that produced "(model returned nothing
            // usable)" in production. Log loudly with everything needed to
            // tell a starved reasoning model from a filtered completion.
            warn!(
                "AI returned empty content in {}ms: model={} {} — raw: {}",
                elapsed_ms,
                self.model,
                out.diagnostics(),
                truncate_for_log(&raw)
            );
        } else {
            info!(
                "AI responded in {}ms: model={} {}",
                elapsed_ms,
                self.model,
                out.diagnostics()
            );
            debug!("AI content: {}", out.content);
        }

        Ok(out)
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
        assert_eq!(first_choice(parsed).unwrap().content, "Nice run.");
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
        assert_eq!(first_choice(parsed).unwrap().content, "");
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
