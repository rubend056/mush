//! OpenAI-compatible chat message and request/response types.
//!
//! These are intentionally loose (`Option` everywhere, `#[serde(default)]`) so
//! that the many "OpenAI-compatible" servers out there all round-trip cleanly.

use serde::{Deserialize, Serialize};

fn function_type() -> String {
    "function".to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    #[serde(default)]
    pub arguments: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "type", default = "function_type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(content.into()),
            ..Default::default()
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.into()),
            ..Default::default()
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: Some(content.into()),
            ..Default::default()
        }
    }

    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            tool_call_id: Some(tool_call_id.into()),
            ..Default::default()
        }
    }

    pub fn text(&self) -> &str {
        self.content.as_deref().unwrap_or("")
    }

    pub fn tool_calls(&self) -> &[ToolCall] {
        self.tool_calls.as_deref().unwrap_or(&[])
    }

    /// Rough size in bytes, used for history budgeting.
    pub fn weight(&self) -> usize {
        let mut n = self.role.len() + self.text().len();
        for call in self.tool_calls() {
            n += call.function.name.len() + call.function.arguments.len() + 16;
        }
        n
    }
}

#[derive(Serialize)]
pub struct ChatRequest<'a> {
    pub model: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [serde_json::Value],
    pub tool_choice: &'a str,
    pub stream: bool,
    pub temperature: f32,
    pub max_tokens: u32,
    /// Provider-specific: enable the model's thinking mode (DeepSeek).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<serde_json::Value>,
    /// Provider-specific: reasoning effort lever (DeepSeek).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChatResponse {
    #[serde(default)]
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub error: Option<ApiError>,
}

#[derive(Debug, Deserialize)]
pub struct Choice {
    #[serde(default)]
    pub message: Message,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ApiError {
    #[serde(default)]
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_params_are_opt_in() {
        let messages = [Message::user("hi")];
        let request = ChatRequest {
            model: "deepseek-flash",
            messages: &messages,
            tools: &[],
            tool_choice: "auto",
            stream: false,
            temperature: 0.2,
            max_tokens: 100,
            thinking: None,
            reasoning_effort: None,
        };
        let plain = serde_json::to_string(&request).unwrap();
        assert!(!plain.contains("thinking"));
        assert!(!plain.contains("reasoning_effort"));

        let request = ChatRequest {
            thinking: Some(serde_json::json!({"type": "enabled"})),
            reasoning_effort: Some("high".to_string()),
            ..request
        };
        let deepseek = serde_json::to_string(&request).unwrap();
        assert!(deepseek.contains("\"thinking\":{\"type\":\"enabled\"}"));
        assert!(deepseek.contains("\"reasoning_effort\":\"high\""));
    }
}
