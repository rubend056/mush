//! OpenAI-compatible chat message and request/response types.
//!
//! These are intentionally loose (`Option` everywhere, `#[serde(default)]`) so
//! that the many "OpenAI-compatible" servers out there all round-trip cleanly.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

fn function_type() -> String {
    "function".to_string()
}

/// Text out of the two shapes a reply's `content` arrives in.
///
/// The spec's request form — and what most endpoints answer with — is a plain
/// string. Newer OpenAI models and several compatible servers answer with an
/// *array of content parts* instead (`[{"type":"text","text":"hi"}]`). Both
/// are the same message, so both parse; a reply is not a `Malformed` one just
/// because a server chose the other spelling.
///
/// Parts are concatenated in order, verbatim, with nothing inserted between
/// them: they are consecutive pieces of one answer, and a separator mush
/// invented would put words in the model's mouth. A part that carries no text
/// (an image URL, a refusal block) contributes nothing.
fn content_from_wire<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<Value>::deserialize(deserializer)?.map(content_text))
}

fn content_text(value: Value) -> String {
    match value {
        Value::String(text) => text,
        // `content: null` is the shape of a pure tool-call reply: no text.
        Value::Null => String::new(),
        Value::Array(parts) => parts.into_iter().map(part_text).collect(),
        // Some servers wrap a lone part in an object instead of an array.
        part @ Value::Object(_) => part_text(part),
        // Nothing else is text, but it is not a reason to drop the reply.
        other => other.to_string(),
    }
}

fn part_text(part: Value) -> String {
    part.get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Every call in one batch needs an id a strict server will accept, and no two
/// calls may share one: a server pairs a result with its call *by id*, so a
/// missing id (mush used to re-send `tool_call_id: ""`) or a duplicate makes
/// the whole pairing invalid and the next request is rejected.
///
/// A missing or repeated id becomes `call_N` — the first `N` at or after the
/// call's position that neither this batch nor an earlier call is already
/// using. Deterministic, and decided only by the batch itself: the same reply
/// always yields the same ids, and a batch that already has unique ids is left
/// exactly as it was.
fn assign_tool_call_ids(calls: &mut [ToolCall]) {
    // Ids this batch already uses, so a synthesized one cannot collide with an
    // id that appears *later* in the same batch.
    let mut used: Vec<String> = calls
        .iter()
        .map(|call| call.id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect();
    let mut answered: Vec<String> = Vec::with_capacity(calls.len());
    for (index, call) in calls.iter_mut().enumerate() {
        let id = call.id.trim().to_string();
        let id = if !id.is_empty() && !answered.contains(&id) {
            id
        } else {
            let mut n = index;
            loop {
                let candidate = format!("call_{n}");
                if !used.contains(&candidate) && !answered.contains(&candidate) {
                    used.push(candidate.clone());
                    break candidate;
                }
                n += 1;
            }
        };
        answered.push(id.clone());
        call.id = id;
    }
}

/// The `tool_calls` wire field, normalized on the way in: a reply is not
/// malformed because a model left an id out or repeated one.
fn tool_calls_from_wire<'de, D>(deserializer: D) -> Result<Option<Vec<ToolCall>>, D::Error>
where
    D: Deserializer<'de>,
{
    let mut calls = Option::<Vec<ToolCall>>::deserialize(deserializer)?;
    if let Some(calls) = calls.as_mut() {
        assign_tool_call_ids(calls);
    }
    Ok(calls)
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
    /// Always a string on the way out — the spec's own request form, for both
    /// assistant history and tool results. On the way in, either wire shape
    /// (see `content_from_wire`).
    #[serde(
        default,
        deserialize_with = "content_from_wire",
        skip_serializing_if = "Option::is_none"
    )]
    pub content: Option<String>,
    /// A thinking model's reasoning for this turn (DeepSeek's
    /// `reasoning_content`). It is read from the reply and written straight
    /// back out with the turn: in thinking mode the endpoint refuses a request
    /// that replays an assistant turn without it, tool-call turns first among
    /// them. `None` for every model that keeps its thinking to itself, and
    /// skipped on the wire then, so no other endpoint ever sees the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(
        default,
        deserialize_with = "tool_calls_from_wire",
        skip_serializing_if = "Option::is_none"
    )]
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

    /// See `assign_tool_call_ids`: every call gets an id a strict server
    /// accepts, and its result can then answer an id that exists. Idempotent,
    /// so a message that already has unique ids comes out unchanged.
    pub fn ensure_tool_call_ids(&mut self) {
        if let Some(calls) = self.tool_calls.as_mut() {
            assign_tool_call_ids(calls);
        }
    }

    /// Rough size in bytes, used for history budgeting. The reasoning is
    /// counted: it goes back out with the turn, so it is part of what the
    /// request costs.
    pub fn weight(&self) -> usize {
        let mut n = self.role.len() + self.text().len();
        if let Some(reasoning) = &self.reasoning_content {
            n += reasoning.len();
        }
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
    /// The reply cap, as the field every OpenAI-compatible endpoint documents.
    /// One of this and `max_completion_tokens` is sent, never both: OpenAI's
    /// reasoning models reject `max_tokens`, and other servers only know it.
    #[serde(skip_serializing_if = "is_zero")]
    pub max_tokens: u32,
    /// The same cap under the newer name, for endpoints that require it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    /// Provider-specific: enable the model's thinking mode (DeepSeek).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<serde_json::Value>,
    /// Provider-specific: reasoning effort lever (DeepSeek).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

#[derive(Debug, Deserialize)]
pub struct ChatResponse {
    #[serde(default)]
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub error: Option<ApiError>,
    /// What the endpoint counted for this request, when it reports it at all.
    /// The only *real* token count mush ever gets: a server that omits it
    /// leaves the bytes-per-token estimate as the one number there is.
    #[serde(default)]
    pub usage: Option<Usage>,
}

/// Token counts as the endpoint reports them.
///
/// Loose like the rest of the module: every field defaults, because servers
/// disagree about which of the three they send — `total_tokens` most of all.
/// A count is read, never invented: nothing here is derived from the text.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
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

    /// A spec-legal reply whose `content` is an array of parts must parse: a
    /// server that sends the newer shape is not a broken endpoint, and dying
    /// with "could not parse model response" on it loses a whole run.
    #[test]
    fn a_reply_parses_from_content_parts_as_well_as_a_string() {
        let plain: Message =
            serde_json::from_str(r#"{"role":"assistant","content":"hi"}"#).unwrap();
        assert_eq!(plain.text(), "hi");

        let parts: Message = serde_json::from_str(
            r#"{"role":"assistant","content":[{"type":"text","text":"hi"},{"type":"text","text":" there"}]}"#,
        )
        .unwrap();
        assert_eq!(parts.text(), "hi there", "parts concatenate in order");

        // A part that is not text (an image, a refusal block) carries none, and
        // a lone part wrapped in an object is still the same answer.
        let mixed: Message = serde_json::from_str(
            r#"{"role":"assistant","content":[{"type":"text","text":"look:"},{"type":"image_url","image_url":{"url":"http://x"}}]}"#,
        )
        .unwrap();
        assert_eq!(mixed.text(), "look:");
        let wrapped: Message =
            serde_json::from_str(r#"{"role":"assistant","content":{"type":"text","text":"hi"}}"#)
                .unwrap();
        assert_eq!(wrapped.text(), "hi");

        // The whole reply, not just the message: this is the parse a model call
        // does, so the parts shape has to survive it too.
        let reply: ChatResponse = serde_json::from_str(
            r#"{"choices":[{"message":{"role":"assistant","content":[{"type":"text","text":"hi"}]},"finish_reason":"stop"}]}"#,
        )
        .unwrap();
        assert_eq!(reply.choices[0].message.text(), "hi");

        // A pure tool-call reply has no text at all, in either spelling.
        let null: Message = serde_json::from_str(r#"{"role":"assistant","content":null}"#).unwrap();
        assert_eq!(null.text(), "");
        let missing: Message = serde_json::from_str(r#"{"role":"assistant"}"#).unwrap();
        assert_eq!(missing.text(), "");
    }

    /// What mush *sends* stays the spec's own wire form: a string. History
    /// re-serialized after a parsed reply must not go back as parts, or the
    /// request shape would depend on which server answered last.
    #[test]
    fn assistant_content_is_written_back_as_a_string() {
        let parsed: Message = serde_json::from_str(
            r#"{"role":"assistant","content":[{"type":"text","text":"hi"},{"type":"text","text":" there"}]}"#,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_string(&parsed).unwrap(),
            r#"{"role":"assistant","content":"hi there"}"#
        );
        assert_eq!(
            serde_json::to_string(&Message::assistant("hi")).unwrap(),
            r#"{"role":"assistant","content":"hi"}"#
        );
    }

    /// A thinking model's reasoning belongs to the turn that produced it, and
    /// has to travel back with it: DeepSeek's thinking mode refuses the next
    /// request when a replayed assistant turn arrives without its
    /// `reasoning_content`, so dropping the field costs the whole conversation.
    #[test]
    fn a_replys_reasoning_is_carried_back_with_its_turn() {
        let reply: ChatResponse = serde_json::from_str(
            r#"{"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":"","reasoning_content":"read the file first","tool_calls":[{"id":"c1","type":"function","function":{"name":"read_file","arguments":"{}"}}]}}]}"#,
        )
        .unwrap();
        let message = &reply.choices[0].message;
        assert_eq!(
            message.reasoning_content.as_deref(),
            Some("read the file first")
        );

        // The history mush sends back is this same message, serialized: the
        // reasoning must be on the wire with the calls it decided.
        let wire = serde_json::to_string(message).unwrap();
        assert!(
            wire.contains(r#""reasoning_content":"read the file first""#),
            "{wire}"
        );

        // It is weighed as part of what the request will cost, or the budget
        // would count a thinking transcript as smaller than it is.
        assert!(message.weight() > Message::assistant("").weight());
    }

    /// A model that does not think must not grow the field by being read: what
    /// goes out is what came in, and nothing else (see `provider_params_are_opt_in`).
    #[test]
    fn a_reply_without_reasoning_does_not_grow_the_field() {
        let from_wire: Message =
            serde_json::from_str(r#"{"role":"assistant","content":"hi"}"#).unwrap();
        assert!(from_wire.reasoning_content.is_none());
        assert_eq!(
            serde_json::to_string(&from_wire).unwrap(),
            r#"{"role":"assistant","content":"hi"}"#
        );
        assert_eq!(
            serde_json::to_string(&Message::assistant("hi")).unwrap(),
            r#"{"role":"assistant","content":"hi"}"#
        );
    }

    /// A call with no id, or two calls sharing one, must not round-trip as
    /// `tool_call_id: ""` / a duplicate: a server pairs a result with its call
    /// by id, and rejects the pairing otherwise.
    #[test]
    fn tool_calls_with_no_id_or_a_duplicate_get_unique_ones() {
        let parsed: Message = serde_json::from_str(
            r#"{"role":"assistant","tool_calls":[
                 {"type":"function","function":{"name":"read_file","arguments":"{}"}},
                 {"id":"dup","type":"function","function":{"name":"ls","arguments":"{}"}},
                 {"id":"dup","type":"function","function":{"name":"ls","arguments":"{}"}},
                 {"id":"   ","type":"function","function":{"name":"ls","arguments":"{}"}}
               ]}"#,
        )
        .unwrap();
        let ids: Vec<&str> = parsed.tool_calls().iter().map(|c| c.id.as_str()).collect();
        assert!(
            ids.iter().all(|id| !id.trim().is_empty()),
            "no call is answered with an empty id: {ids:?}"
        );
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "ids are unique: {ids:?}");
        // Deterministic, and the first user of an id keeps it.
        assert_eq!(ids, ["call_0", "dup", "call_2", "call_3"]);

        // A synthesized id never collides with one already in the batch.
        let clash: Message = serde_json::from_str(
            r#"{"role":"assistant","tool_calls":[
                 {"type":"function","function":{"name":"ls","arguments":"{}"}},
                 {"id":"call_0","type":"function","function":{"name":"ls","arguments":"{}"}}
               ]}"#,
        )
        .unwrap();
        let ids: Vec<&str> = clash.tool_calls().iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["call_1", "call_0"], "the existing id is never taken");
    }

    /// Normalizing is idempotent: a batch that already has unique ids — every
    /// call mush got from a well-behaved endpoint — is left exactly as it was.
    #[test]
    fn unique_tool_call_ids_are_left_alone() {
        let mut message: Message = serde_json::from_str(
            r#"{"role":"assistant","tool_calls":[
                 {"id":"call_a","type":"function","function":{"name":"ls","arguments":"{}"}},
                 {"id":"call_1","type":"function","function":{"name":"ls","arguments":"{}"}}
               ]}"#,
        )
        .unwrap();
        let before = serde_json::to_string(&message).unwrap();
        message.ensure_tool_call_ids();
        message.ensure_tool_call_ids();
        assert_eq!(serde_json::to_string(&message).unwrap(), before);
    }

    /// A reply may report its own token counts; what mush *sends* never does,
    /// so a parsed one is not re-serialized (the response type is only ever
    /// read from the wire).
    #[test]
    fn usage_is_kept_and_defaults_when_a_server_omits_it() {
        let reply: ChatResponse = serde_json::from_str(
            r#"{"choices":[],"usage":{"prompt_tokens":1200,"completion_tokens":34,"total_tokens":1234}}"#,
        )
        .unwrap();
        assert_eq!(
            reply.usage,
            Some(Usage {
                prompt_tokens: 1200,
                completion_tokens: 34,
                total_tokens: 1234,
            })
        );
        // A server that sends none, or only some of the three, is not an error.
        let bare: ChatResponse = serde_json::from_str(r#"{"choices":[]}"#).unwrap();
        assert_eq!(bare.usage, None);
        let partial: ChatResponse =
            serde_json::from_str(r#"{"choices":[],"usage":{"completion_tokens":7}}"#).unwrap();
        assert_eq!(
            partial.usage,
            Some(Usage {
                prompt_tokens: 0,
                completion_tokens: 7,
                total_tokens: 0,
            })
        );
    }

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
            max_completion_tokens: None,
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
