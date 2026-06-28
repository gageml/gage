#![allow(unexpected_cfgs)]
//! Anthropic Messages API client for the Rune scanner runtime.
//! Narrow surface: one POST to `/v1/messages`, no streaming.

use std::env;
use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub enum LlmError {
    Config(String),
    Network(String),
    Http { status: u16, body: String },
    Decode(String),
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(msg) => write!(f, "config: {msg}"),
            Self::Network(msg) => write!(f, "network: {msg}"),
            Self::Http { status, body } => write!(f, "http {status}: {body}"),
            Self::Decode(msg) => write!(f, "decode: {msg}"),
        }
    }
}

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";
#[cfg(feature = "oauth")]
const OAUTH_BETA: &str = "oauth-2025-04-20";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub kind: String,
}

impl CacheControl {
    pub fn ephemeral() -> Self {
        Self {
            kind: "ephemeral".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemBlock {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

impl SystemBlock {
    pub fn text_cached(text: String) -> Self {
        Self {
            kind: "text".into(),
            text,
            cache_control: Some(CacheControl::ephemeral()),
        }
    }
}

const MODEL_ALIASES: &[(&str, &str)] = &[
    ("opus", "claude-opus-4-8"),
    ("large", "claude-opus-4-8"),
    ("sonnet", "claude-sonnet-4-6"),
    ("medium", "claude-sonnet-4-6"),
    ("haiku", "claude-haiku-4-5"),
    ("small", "claude-haiku-4-5"),
];

pub fn resolve_model(input: &str) -> &str {
    for (alias, target) in MODEL_ALIASES {
        if *alias == input {
            return target;
        }
    }
    input
}

#[derive(Debug, Serialize)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u32,
    pub system: Vec<SystemBlock>,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct MessagesResponse {
    pub id: String,
    pub model: String,
    pub role: String,
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    #[serde(default)]
    pub stop_sequence: Option<String>,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        cache_control: Option<CacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
    },
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    PauseTurn,
    Refusal,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[allow(dead_code)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
    #[serde(default)]
    pub cache_read_input_tokens: u32,
}

#[async_trait]
pub trait Client: Send + Sync {
    async fn messages(&self, req: &MessagesRequest) -> Result<MessagesResponse, LlmError>;
}

#[cfg(feature = "oauth")]
enum Credential {
    ApiKey(String),
    OAuthBearer(String),
}

pub struct HttpClient {
    http: reqwest::Client,
    #[cfg(feature = "oauth")]
    credential: Credential,
    #[cfg(not(feature = "oauth"))]
    api_key: String,
}

impl HttpClient {
    pub fn from_env() -> Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| LlmError::Config(format!("build reqwest client: {e}")))?;

        #[cfg(feature = "oauth")]
        {
            let credential = if let Ok(key) = env::var("ANTHROPIC_API_KEY") {
                Credential::ApiKey(key)
            } else {
                Credential::OAuthBearer(
                    read_claude_oauth_token().map_err(|e| LlmError::Config(e.to_string()))?,
                )
            };
            Ok(Self { http, credential })
        }

        #[cfg(not(feature = "oauth"))]
        {
            let api_key = env::var("ANTHROPIC_API_KEY").map_err(|_e| {
                LlmError::Config("ANTHROPIC_API_KEY environment variable is required".into())
            })?;
            Ok(Self { http, api_key })
        }
    }
}

#[cfg(feature = "oauth")]
fn read_claude_oauth_token() -> io::Result<String> {
    let home = env::var("HOME").map_err(|_env_err| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "ANTHROPIC_API_KEY is not set and HOME is not set (needed to find Claude Code credentials)",
        )
    })?;
    let path = std::path::Path::new(&home)
        .join(".claude")
        .join(".credentials.json");
    let contents = std::fs::read_to_string(&path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "ANTHROPIC_API_KEY is not set and cannot read {}: {e}",
                path.display()
            ),
        )
    })?;
    let json: Value = serde_json::from_str(&contents).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("cannot parse {}: {e}", path.display()),
        )
    })?;
    json.pointer("/claudeAiOauth/accessToken")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "ANTHROPIC_API_KEY is not set and {} does not contain claudeAiOauth.accessToken",
                    path.display()
                ),
            )
        })
}

#[async_trait]
impl Client for HttpClient {
    async fn messages(&self, req: &MessagesRequest) -> Result<MessagesResponse, LlmError> {
        let mut builder = self.http.post(ANTHROPIC_API_URL);

        #[cfg(feature = "oauth")]
        {
            builder = match &self.credential {
                Credential::ApiKey(key) => builder.header("x-api-key", key),
                Credential::OAuthBearer(token) => builder
                    .header("authorization", format!("Bearer {token}"))
                    .header("anthropic-beta", OAUTH_BETA),
            };
        }

        #[cfg(not(feature = "oauth"))]
        {
            builder = builder.header("x-api-key", &self.api_key);
        }

        let resp = builder
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .json(req)
            .send()
            .await
            .map_err(|e| LlmError::Network(format!("{e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::Http {
                status: status.as_u16(),
                body,
            });
        }
        resp.json::<MessagesResponse>()
            .await
            .map_err(|e| LlmError::Decode(format!("{e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_model_substitutes_aliases() {
        assert_eq!(resolve_model("opus"), "claude-opus-4-8");
        assert_eq!(resolve_model("large"), "claude-opus-4-8");
        assert_eq!(resolve_model("sonnet"), "claude-sonnet-4-6");
        assert_eq!(resolve_model("medium"), "claude-sonnet-4-6");
        assert_eq!(resolve_model("haiku"), "claude-haiku-4-5");
        assert_eq!(resolve_model("small"), "claude-haiku-4-5");
    }

    #[test]
    fn resolve_model_passes_through_unknown() {
        assert_eq!(
            resolve_model("claude-opus-4-7-20260101"),
            "claude-opus-4-7-20260101"
        );
        assert_eq!(resolve_model("claude-future-1-0"), "claude-future-1-0");
    }

    #[test]
    fn content_block_round_trip_tool_use() {
        let block = ContentBlock::ToolUse {
            id: "tu_1".into(),
            name: "emit_friction_finding".into(),
            input: serde_json::json!({"value": "x", "target": 42}),
        };
        let s = serde_json::to_string(&block).unwrap();
        assert!(s.contains(r#""type":"tool_use""#));
        let back: ContentBlock = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, ContentBlock::ToolUse { .. }));
    }

    #[test]
    fn content_block_round_trip_tool_result() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "tu_1".into(),
            content: "ok".into(),
            is_error: false,
        };
        let s = serde_json::to_string(&block).unwrap();
        assert!(s.contains(r#""type":"tool_result""#));
        assert!(!s.contains("is_error"));
        let back: ContentBlock = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, ContentBlock::ToolResult { .. }));
    }

    #[test]
    fn content_block_serializes_is_error_when_true() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "tu_1".into(),
            content: "missing target".into(),
            is_error: true,
        };
        let s = serde_json::to_string(&block).unwrap();
        assert!(s.contains(r#""is_error":true"#));
    }

    #[test]
    fn stop_reason_deserializes_known_values() {
        let pairs = [
            ("\"end_turn\"", StopReason::EndTurn),
            ("\"tool_use\"", StopReason::ToolUse),
            ("\"max_tokens\"", StopReason::MaxTokens),
            ("\"stop_sequence\"", StopReason::StopSequence),
            ("\"pause_turn\"", StopReason::PauseTurn),
            ("\"refusal\"", StopReason::Refusal),
        ];
        for (s, expected) in pairs {
            let got: StopReason = serde_json::from_str(s).unwrap();
            assert_eq!(got, expected);
        }
    }
}
