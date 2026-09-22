//! OpenAI-compatible chat message and request/response types.
//!
//! These are intentionally loose (`Option` everywhere, `#[serde(default)]`) so
//! that the many "OpenAI-compatible" servers out there all round-trip cleanly.

use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
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
///
/// An `image_url` part is dropped, deliberately. This is the *reply* path and
/// no OpenAI-compatible endpoint sends an image back today — a model answers
/// in text — and a round trip cannot recover one properly even if it did: the
/// data URL carries the mime and the bytes, but not the *path*, which is the
/// one fact a placeholder must name (see [`placeholder`]). Decoding a
/// pathless image back into [`Message::images`] would mean inventing a name
/// for it, which is worse than keeping the text and no image. So nothing is
/// re-decoded here, and a message that carried an image comes back as its
/// text.
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

/// One image carried inside a message: a screenshot, a chart, a rendered
/// diagram the model is being asked to look at.
///
/// The bytes live in the message rather than behind a URL because mush has no
/// server to put them on: a request is the only thing that leaves this
/// machine, so anything the model is to look at has to travel in it. `path` is
/// workspace-relative, the name the producer read it from — the one fact that
/// makes the image findable again once the bytes are gone (a trimmed history
/// or a saved session keeps the path and drops the bytes). `mime` is what the
/// `data:` URL tells the endpoint the bytes are. `pixels` is what the picture
/// *costs*, which is a different fact from how many bytes it took to write:
/// [`Image::weight`] prices an image by this, so a 70 KB and a 724 KB
/// screenshot of the same size weigh the same.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Image {
    pub path: String,
    pub mime: String,
    pub bytes: Vec<u8>,
    /// The picture's width and height, read from its own header
    /// ([`crate::workspace::image_dimensions`]) when it was read from disk.
    ///
    /// `None` when no size could be read — a truncated file, a format whose
    /// header carries none, bytes that are not what the mime claims — and then
    /// [`Image::weight`] falls back to the raw byte count, which errs high for
    /// a picture: the safe direction, because an over-weight estimate sheds an
    /// image's payload before it drops a turn.
    ///
    /// `serde(default)` so an `Image` stored before this field existed still
    /// reads as "size unknown" instead of failing. Nothing mush writes carries
    /// one today — a session sheds images before saving, and a request spells
    /// them as `data:` URLs — so this is a shape for hand-built payloads, and
    /// a missing field must never be the thing that loses a session.
    #[serde(default)]
    pub pixels: Option<(u32, u32)>,
}

impl Image {
    /// What this picture costs the context budget, in the byte-shaped currency
    /// [`Message::weight`] counts — its pixels at the
    /// [`PIXELS_PER_TOKEN`](crate::config::PIXELS_PER_TOKEN) rule, or its raw
    /// bytes when no header named a size, plus the path and mime that travel
    /// with it.
    ///
    /// Pixels are the estimate because pixels are what a vision endpoint's
    /// price is made of: a 1920×1080 screenshot is ~2.8k tokens whether it is
    /// written as a 724 KB png or a 70 KB one, where its file size alone used
    /// to read as ~247k ([`crate::config::tokens_for_pixels`] turns the
    /// picture into tokens, and [`BYTES_PER_TOKEN`](crate::config::BYTES_PER_TOKEN)
    /// turns them back, so text and pictures stay in one currency).
    pub fn weight(&self) -> usize {
        let payload = match self.pixels {
            Some((width, height)) => {
                crate::config::tokens_for_pixels(u64::from(width) * u64::from(height))
                    .saturating_mul(crate::config::BYTES_PER_TOKEN)
            }
            // No size to read: the bytes are the fallback, and for a picture
            // they overcount, which is the safe direction (see the field).
            None => self.bytes.len(),
        };
        // Saturating, because a header may claim `u32::MAX × u32::MAX` pixels:
        // the estimate is then as large as a `usize` can hold on a 32-bit
        // target, and adding the path on top must not be the thing that
        // panics. A number that saturates still says "too big to fit" as
        // clearly as one that does not.
        payload
            .saturating_add(self.path.len())
            .saturating_add(self.mime.len())
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Message {
    pub role: String,
    /// The message's text. A plain JSON string on the way out — the spec's own
    /// request form, for both assistant history and tool results — unless the
    /// message carries images, when it is the content array
    /// ([`Message::content_parts`]). On the way in, either wire shape (see
    /// `content_from_wire`).
    #[serde(default, deserialize_with = "content_from_wire")]
    pub content: Option<String>,
    /// Images carried *in* this message, in the order they were attached.
    /// They are not a wire field of their own: they become `image_url` parts
    /// *inside* `content` on the way out ([`Message::content_parts`]), and a
    /// part that carries no text contributes nothing on the way in
    /// (`content_from_wire`). A message that came from an endpoint or from a
    /// stored session therefore has none: a session never writes the bytes
    /// ([`Message::drop_images`]), so there is nothing of them to read back.
    #[serde(default, skip_deserializing)]
    pub images: Vec<Image>,
    /// A thinking model's reasoning for this turn (`reasoning_content`). It is
    /// read from the reply and written straight
    /// back out with the turn: in thinking mode the endpoint refuses a request
    /// that replays an assistant turn without it, tool-call turns first among
    /// them. `None` for every model that keeps its thinking to itself, and
    /// skipped on the wire then, so no other endpoint ever sees the field.
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default, deserialize_with = "tool_calls_from_wire")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

/// `Message` is serialized by hand, and only because of `images`.
///
/// Every field comes out exactly as the derive used to write it — same names,
/// same order, same "absent when `None`" rule — so a message with no images is
/// byte for byte what this type has always sent, whatever the request around
/// it. What the derive cannot express is that the *shape* of `content` depends
/// on a sibling: the spec's plain string, or, when images ride along, its
/// content array. `serialize_with` on a field receives only that field, and
/// the alternative — a second `Message`-shaped type — is exactly what the
/// request path must not grow: [`ChatRequest`] takes `&[Message]` and never
/// learns there are images.
impl Serialize for Message {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let fields = 1
            + usize::from(self.content.is_some() || !self.images.is_empty())
            + usize::from(self.reasoning_content.is_some())
            + usize::from(self.tool_calls.is_some())
            + usize::from(self.tool_call_id.is_some());
        let mut message = serializer.serialize_struct("Message", fields)?;
        message.serialize_field("role", &self.role)?;
        if self.images.is_empty() {
            if let Some(content) = &self.content {
                message.serialize_field("content", content)?;
            }
        } else {
            message.serialize_field("content", &self.content_parts())?;
        }
        if let Some(reasoning) = &self.reasoning_content {
            message.serialize_field("reasoning_content", reasoning)?;
        }
        if let Some(calls) = &self.tool_calls {
            message.serialize_field("tool_calls", calls)?;
        }
        if let Some(id) = &self.tool_call_id {
            message.serialize_field("tool_call_id", id)?;
        }
        message.end()
    }
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

    /// A tool result that carries images as well as its text — what `read_file`
    /// answers with when the file is a picture. One constructor so the shape a
    /// vision endpoint reads (text part, then one image part each) has one
    /// home, and so the tool loop that builds the message cannot pair the wrong
    /// text with the wrong bytes.
    pub fn tool_with_images(
        tool_call_id: impl Into<String>,
        content: impl Into<String>,
        images: Vec<Image>,
    ) -> Self {
        Self {
            images,
            ..Self::tool(tool_call_id, content)
        }
    }

    /// The human's own message when it carries images: the words they typed,
    /// and the pictures they attached. The counterpart of
    /// [`Self::tool_with_images`] and the same reason for existing — the text
    /// part comes first and one `image_url` part follows each image, and the
    /// one place that order is built is here.
    pub fn user_with_images(text: impl Into<String>, images: Vec<Image>) -> Self {
        Self {
            images,
            ..Self::user(text)
        }
    }

    pub fn text(&self) -> &str {
        self.content.as_deref().unwrap_or("")
    }

    pub fn tool_calls(&self) -> &[ToolCall] {
        self.tool_calls.as_deref().unwrap_or(&[])
    }

    /// The `content` a message with images goes out as: the text first (omitted
    /// when there is none), then one `image_url` part per image, each holding a
    /// `data:` URL. This is the vision form of the spec, and the only way an
    /// image rides in a request: [`ChatRequest`] knows nothing about it.
    fn content_parts(&self) -> Vec<ContentPart<'_>> {
        let mut parts = Vec::with_capacity(self.images.len() + 1);
        if !self.text().is_empty() {
            parts.push(ContentPart {
                kind: "text",
                text: Some(self.text()),
                url: None,
            });
        }
        parts.extend(self.images.iter().map(|image| ContentPart {
            kind: "image_url",
            text: None,
            url: Some(data_url(image)),
        }));
        parts
    }

    /// Shed this message's image payloads, leaving one [`placeholder`] line
    /// where each was, so the transcript still says an image was there and
    /// which file it came from — and the model can read that file again if it
    /// needs the image.
    ///
    /// The one way an image leaves a live message, called by both the trimmer
    /// (before it drops a whole turn) and the session writer (before it writes
    /// a file). Idempotent on purpose: a message that already lost its images
    /// has nothing left to shed, so re-saving a loaded session cannot stack a
    /// second placeholder on the first one's text.
    pub fn drop_images(&mut self) {
        if self.images.is_empty() {
            return;
        }
        let dropped = self
            .images
            .drain(..)
            .map(|image| placeholder(&image))
            .collect::<Vec<_>>()
            .join("\n");
        self.content = Some(match self.content.take() {
            Some(text) if !text.is_empty() => format!("{text}\n{dropped}"),
            _ => dropped,
        });
    }

    /// Rough size in bytes, used for history budgeting. The reasoning is
    /// counted: it goes back out with the turn, so it is part of what the
    /// request costs. An image is counted by [`Image::weight`] — its pixels at
    /// the [`PIXELS_PER_TOKEN`](crate::config::PIXELS_PER_TOKEN) rule, or its
    /// raw bytes when its header named no size — plus the path and mime that
    /// travel with it. Base64's 4/3 inflation is deliberately *not* modeled:
    /// that is what the transport carries, not what the endpoint charges (the
    /// one caveat, an endpoint that tokenized the `data:` text itself, is
    /// stated on the constant and not modeled).
    pub fn weight(&self) -> usize {
        let mut n = self.role.len() + self.text().len();
        if let Some(reasoning) = &self.reasoning_content {
            n += reasoning.len();
        }
        for call in self.tool_calls() {
            n += call.function.name.len() + call.function.arguments.len() + 16;
        }
        for image in &self.images {
            n = n.saturating_add(image.weight());
        }
        n
    }
}

/// One part of the content array an image message goes out as: the text, or
/// one image's `data:` URL.
///
/// Hand-written for the same kind of reason [`Message`]'s serializer is: the
/// obvious carrier, `serde_json::Value`, holds the same object with its keys
/// *sorted* (`preserve_order` is a cargo feature, and turning it on would pull
/// a crate into the tree this repo's budget does not allow). `type` comes
/// first here because that is the order the vision form documents, so a
/// request read in a log looks like the shape an endpoint expects.
struct ContentPart<'a> {
    /// The part's `type`: `text` or `image_url`.
    kind: &'static str,
    text: Option<&'a str>,
    url: Option<String>,
}

impl Serialize for ContentPart<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut part = serializer.serialize_struct("content_part", 2)?;
        part.serialize_field("type", self.kind)?;
        if let Some(text) = self.text {
            part.serialize_field("text", text)?;
        }
        if let Some(url) = &self.url {
            // One key, so the sorted map cannot reorder anything that matters.
            part.serialize_field("image_url", &serde_json::json!({"url": url}))?;
        }
        part.end()
    }
}

/// The `data:` URL an image travels as: its mime, then its bytes in standard
/// base64. An image is handed to the endpoint as text because that is the one
/// image spelling OpenAI-compatible vision endpoints document.
fn data_url(image: &Image) -> String {
    format!("data:{};base64,{}", image.mime, base64_encode(&image.bytes))
}

/// The line a dropped image leaves behind, in the one spelling the trimmer and
/// the session writer both use (never two). It names the path, because that is
/// what makes the image reachable again — the model can read the file — and the
/// format, so the line cannot be mistaken for something the model said.
fn placeholder(image: &Image) -> String {
    // `image/png` prints as `png`: the mime already leads with the fact that
    // this is an image, and the sentence has room for one noun.
    let format = image.mime.strip_prefix("image/").unwrap_or(&image.mime);
    format!(
        "[image: {} ({format}) — bytes dropped to save room; read the file again if you need them]",
        image.path
    )
}

/// Standard base64 (RFC 4648 §4: `A–Z a–z 0–9 + /`, `=` padding, no line
/// breaks) — the alphabet a `data:` URL carries an image in.
///
/// Hand-rolled rather than pulled in as a crate: this repo's dependency budget
/// is a rule (§7), it has exactly one caller ([`data_url`]), and a dependency
/// for thirty lines of table lookup is a tree of code to audit for one
/// function. Private to this module because this is the only place bytes need
/// to become text.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut whole = bytes.chunks_exact(3);
    for chunk in &mut whole {
        // `chunks_exact(3)` hands over exactly three bytes, so these indices
        // cannot leave the slice.
        let n = u32::from(chunk[0]) << 16 | u32::from(chunk[1]) << 8 | u32::from(chunk[2]);
        for shift in [18, 12, 6, 0] {
            out.push(char::from(ALPHABET[(n >> shift) as usize & 0x3f]));
        }
    }
    // One or two bytes are left: pad them out to three with zero bits, then
    // write `=` for each character that has no input bit behind it.
    let tail = whole.remainder();
    if !tail.is_empty() {
        let mut chunk = [0u8; 3];
        chunk[..tail.len()].copy_from_slice(tail);
        let n = u32::from(chunk[0]) << 16 | u32::from(chunk[1]) << 8 | u32::from(chunk[2]);
        for (slot, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if slot <= tail.len() {
                out.push(char::from(ALPHABET[(n >> shift) as usize & 0x3f]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Words become a **user** message, because that is the only speaker words
/// without a role can be: a nudge the human typed, a brief a test hands an
/// actor. It exists so the call sites that used to pass a bare `String` — the
/// nudge road, which grew images and is a [`Message`] now — read as the message
/// they always were rather than as a conversion each one spells out.
impl From<&str> for Message {
    fn from(text: &str) -> Self {
        Self::user(text)
    }
}

impl From<String> for Message {
    fn from(text: String) -> Self {
        Self::user(text)
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
    /// Provider-specific: enable the model's thinking mode, for the providers
    /// whose row in `provider::PROVIDERS` asks for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<serde_json::Value>,
    /// Provider-specific: the reasoning effort lever, sent only where a
    /// provider documents it (see `provider::PROVIDERS`).
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
    use crate::config::{BYTES_PER_TOKEN, PIXELS_PER_TOKEN};

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

    /// Normalizing happens on the way in and only there: a batch that already
    /// has unique ids — every call mush got from a well-behaved endpoint — is
    /// left exactly as it was.
    #[test]
    fn unique_tool_call_ids_are_left_alone() {
        let message: Message = serde_json::from_str(
            r#"{"role":"assistant","tool_calls":[
                 {"id":"call_a","type":"function","function":{"name":"ls","arguments":"{}"}},
                 {"id":"call_1","type":"function","function":{"name":"ls","arguments":"{}"}}
               ]}"#,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_string(&message).unwrap(),
            r#"{"role":"assistant","tool_calls":[{"id":"call_a","type":"function","function":{"name":"ls","arguments":"{}"}},{"id":"call_1","type":"function","function":{"name":"ls","arguments":"{}"}}]}"#
        );
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

    /// The bytes mush puts in a `data:` URL, against the RFC 4648 §10 vectors
    /// and the tails: one and two spare bytes (two `=` and one `=`), a byte
    /// value from the high half of the alphabet (a PNG's magic number and a
    /// JPEG's both start there), and the 57-byte boundary — nineteen whole
    /// triples, where padding stops and starts again.
    #[test]
    fn base64_encodes_the_standard_vectors_and_pads_the_tail() {
        assert_eq!(base64_encode(b""), "", "nothing encodes to nothing");
        assert_eq!(base64_encode(b"f"), "Zg==", "one spare byte pads twice");
        assert_eq!(base64_encode(b"fo"), "Zm8=", "two spare bytes pad once");
        assert_eq!(
            base64_encode(b"foo"),
            "Zm9v",
            "three bytes fill four characters"
        );
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(
            base64_encode(&[0xFF, 0xFE]),
            "//4=",
            "`+` and `/` are reachable, and the last character is padding"
        );

        let fifty_seven = vec![0u8; 57];
        assert_eq!(
            base64_encode(&fifty_seven),
            "AAAA".repeat(19),
            "57 bytes is nineteen whole triples: no padding"
        );
        let fifty_eight = vec![0u8; 58];
        assert_eq!(
            base64_encode(&fifty_eight),
            format!("{}AA==", "AAAA".repeat(19)),
            "58 bytes crosses that boundary: one spare byte and two pads"
        );
    }

    /// The wire form of an image: `content` stops being a string and becomes
    /// the spec's content array — the text first, then one `image_url` part
    /// holding the mime and the base64 of the bytes. This is the shape the
    /// brief pins, and it is the whole reason `Message` has a hand-written
    /// serializer.
    #[test]
    fn an_image_rides_inside_content_as_a_data_url() {
        let mut message = Message::user("what is this?");
        message.images.push(tiny_image());
        assert_eq!(
            serde_json::to_string(&message).unwrap(),
            r#"{"role":"user","content":[{"type":"text","text":"what is this?"},{"type":"image_url","image_url":{"url":"data:image/png;base64,//4="}}]}"#
        );

        // No text, no text part: a message that is only an image is still a
        // legal request, and an empty `{"type":"text","text":""}` part
        // would be a part the endpoint has to read for nothing.
        let mut silent = Message::user("");
        silent.images.push(tiny_image());
        assert_eq!(
            serde_json::to_string(&silent).unwrap(),
            r#"{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,//4="}}]}"#
        );
        let mut wordless = Message::tool("call_0", "");
        wordless.content = None;
        wordless.images.push(tiny_image());
        assert_eq!(
            serde_json::to_string(&wordless).unwrap(),
            r#"{"role":"tool","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,//4="}}],"tool_call_id":"call_0"}"#
        );
    }

    /// The human's own message is built the same way a tool result is, from
    /// the one constructor that owns the text-then-images order — and a plain
    /// user message still goes out as the spec's JSON string, byte for byte as
    /// it always did.
    #[test]
    fn a_user_message_with_images_is_the_vision_content_array() {
        let message = Message::user_with_images("what is this?", vec![tiny_image()]);
        assert_eq!(message.role, "user");
        assert_eq!(
            serde_json::to_string(&message).unwrap(),
            r#"{"role":"user","content":[{"type":"text","text":"what is this?"},{"type":"image_url","image_url":{"url":"data:image/png;base64,//4="}}]}"#
        );
        assert_eq!(
            serde_json::to_string(&Message::user("just words")).unwrap(),
            r#"{"role":"user","content":"just words"}"#,
            "no images, no content array"
        );
    }

    /// Words become a user message wherever words become a message: the nudge
    /// road passes a [`Message`] now, and `.into()` is how a test or a caller
    /// that holds only words spells it.
    #[test]
    fn words_convert_to_a_user_message() {
        assert_eq!(Message::from("hi").text(), "hi");
        assert_eq!(Message::from(String::from("hi")).text(), "hi");
        assert_eq!(Message::from("hi").role, "user");
    }

    /// The request struct carries `&[Message]` and nothing else: an image rides
    /// in the content of a message, so the whole request body has no `images`
    /// key, and `ChatRequest` never learns that images exist.
    #[test]
    fn a_request_carrying_an_image_has_no_images_field() {
        let mut message = Message::user("what is this?");
        message.images.push(tiny_image());
        let messages = [message];
        let request = ChatRequest {
            model: "vision-model",
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
        let wire = serde_json::to_string(&request).unwrap();
        assert!(
            wire.contains(
                r#""content":[{"type":"text","text":"what is this?"},{"type":"image_url","image_url":{"url":"data:image/png;base64,//4="}}]"#
            ),
            "the image is in the content of the request's message: {wire}"
        );
        assert!(
            !wire.contains("\"images\""),
            "the request struct must not learn about images: {wire}"
        );
    }

    /// A round trip through the wire keeps the text and no image. No
    /// OpenAI-compatible endpoint answers with an image part today, and one
    /// that did could not be decoded back *properly*: the data URL carries the
    /// mime and the bytes but not the path, which is the one fact a placeholder
    /// must name — so the bytes are dropped rather than given an invented name.
    #[test]
    fn an_image_that_comes_back_from_the_wire_is_text_without_bytes() {
        let mut message = Message::user("what is this?");
        message.images.push(tiny_image());
        let wire = serde_json::to_string(&message).unwrap();
        let back: Message = serde_json::from_str(&wire).unwrap();
        assert_eq!(back.text(), "what is this?", "the text survives the trip");
        assert!(
            back.images.is_empty(),
            "the bytes have no path to come back to, so they do not come back"
        );

        // `images` is not a field any file or endpoint writes: one that is
        // there by hand is ignored, so a session cannot resurrect payloads.
        let hand_edited: Message = serde_json::from_str(
            r#"{"role":"user","content":"hi","images":[{"path":"x","mime":"image/png","bytes":[1,2]}]}"#,
        )
        .unwrap();
        assert_eq!(hand_edited.text(), "hi");
        assert!(hand_edited.images.is_empty(), "no `images` key is a field");
    }

    /// The placeholder is the one spelling both a trimmed history and a saved
    /// session leave: it names the path (so the model can read the file again)
    /// and the format, and dropping twice adds nothing — the idempotence a
    /// second save of a loaded session depends on.
    #[test]
    fn a_dropped_image_leaves_a_placeholder_naming_its_path() {
        let mut message = Message::user("what is this?");
        message.images.push(tiny_image());
        message.drop_images();
        assert_eq!(
            message.text(),
            "what is this?\n[image: shots/tiny.png (png) — bytes dropped to save room; read the file again if you need them]"
        );
        assert!(message.images.is_empty(), "the bytes are gone");

        let once = message.text().to_string();
        message.drop_images();
        assert_eq!(
            message.text(),
            once,
            "a second drop is a no-op, not a second placeholder"
        );

        // A message whose whole content was the image keeps the placeholder as
        // the content, with no leading newline.
        let mut silent = Message::user("");
        silent.images.push(tiny_image());
        silent.drop_images();
        assert_eq!(
            silent.text(),
            "[image: shots/tiny.png (png) — bytes dropped to save room; read the file again if you need them]"
        );
    }

    /// The fallback: an image whose header named no size is the bytes it took
    /// to write, plus the path and mime that travel with them — which
    /// overcounts a picture, the safe direction. A transcript that carried
    /// such an image is not the size of its text, and a trimmer that thought
    /// it was would never shed the payload that actually exceeds the window.
    #[test]
    fn an_image_with_no_size_in_its_header_weighs_its_bytes_and_the_path_and_mime() {
        let mut message = Message::user("look");
        let text_only = message.weight();
        let image = tiny_image();
        assert_eq!(image.pixels, None, "there is no header in these bytes");
        let extra = image.bytes.len() + image.path.len() + image.mime.len();
        message.images.push(image);
        assert_eq!(
            message.weight(),
            text_only + extra,
            "bytes, path and mime are all part of what has to fit the window"
        );
    }

    /// The ruling this shape exists for: a 724 KiB, 1920×1080 screenshot reads
    /// as ~2.8k tokens, not the ~247k its file size used to charge — pixels
    /// are what a vision endpoint prices, and the file's bytes are what the
    /// transport carries. The margin covers the path and mime that ride on
    /// top; the comparison against the old count is what makes the fix a fix.
    #[test]
    fn a_screenshot_weighs_its_pixels_not_its_bytes() {
        let mut message = Message::user("look");
        let text_only = message.weight();
        message.images.push(Image {
            path: "shots/screen.png".into(),
            mime: "image/png".into(),
            bytes: vec![0x41; 741_396],
            pixels: Some((1_920, 1_080)),
        });

        let tokens = (message.weight() - text_only) / BYTES_PER_TOKEN;
        let by_pixels = (1_920 * 1_080) / PIXELS_PER_TOKEN;
        let by_bytes = 741_396 / BYTES_PER_TOKEN;
        assert!(
            (by_pixels..=by_pixels + 16).contains(&tokens),
            "{tokens} tokens for a 1920×1080 screenshot, not ~{by_pixels} plus its path"
        );
        assert!(
            tokens * 50 < by_bytes,
            "the old byte count was ~100× high ({by_bytes}), and this is {tokens}"
        );
    }

    /// The fallback is a promise, said in its own words: an image whose header
    /// named no size weighs its bytes — the erring-high road — so nothing a
    /// truncated file can say makes a picture cheap.
    #[test]
    fn an_image_with_unknown_pixels_weighs_its_bytes_the_erring_high_road() {
        let image = Image {
            path: "shots/cut.png".into(),
            mime: "image/png".into(),
            bytes: vec![0x41; 500_000],
            pixels: None,
        };
        assert_eq!(
            image.weight(),
            image.bytes.len() + image.path.len() + image.mime.len(),
            "unknown pixels fall back to the bytes, and the fallback errs high"
        );
        // The pixels rule would have said ~2k tokens; the bytes say ~166k.
        assert!(image.weight() / BYTES_PER_TOKEN > 160_000);
    }

    /// A header can claim the largest size a pair of `u32`s can hold, and the
    /// accounting must not wrap around it: the pixel count is taken in `u64`,
    /// the product and the sums in saturating `usize`, so the picture weighs
    /// "as much as there is" rather than a small number come around again.
    /// The expectation is computed in `u128`, so the test does not repeat the
    /// arithmetic it is checking.
    #[test]
    fn the_largest_pixels_a_header_can_claim_do_not_overflow_the_weight() {
        let image = Image {
            path: "shots/max.png".into(),
            mime: "image/png".into(),
            bytes: vec![],
            pixels: Some((u32::MAX, u32::MAX)),
        };
        let pixels = u128::from(u32::MAX) * u128::from(u32::MAX);
        let tokens = pixels.div_ceil(PIXELS_PER_TOKEN as u128);
        let expected = usize::try_from(tokens * BYTES_PER_TOKEN as u128)
            .unwrap_or(usize::MAX)
            .saturating_add(image.path.len() + image.mime.len());
        assert_eq!(image.weight(), expected);
        // On a 64-bit target the number is exact, and it is not small.
        #[cfg(target_pointer_width = "64")]
        assert_eq!(image.weight(), 73_786_976_260_478_491);

        // In a message, and in a transcript-sized sum, the same image cannot
        // take the total around either.
        let mut message = Message::user("x".repeat(1_000));
        message.images.push(image.clone());
        assert_eq!(
            message.weight(),
            "user".len() + 1_000 + image.weight(),
            "the image is added to the text, not wrapped into it"
        );
        let total: usize = std::iter::repeat(message)
            .take(4)
            .map(|message| message.weight())
            .fold(0, usize::saturating_add);
        assert!(total >= image.weight(), "four of them are at least one");

        // The conversion at the top of its own range: saturating, never a
        // wrap to zero.
        assert!(crate::config::tokens_for_pixels(u64::MAX) > 0);
    }

    /// The one image the image tests are about: two bytes, a path and a mime,
    /// small enough to read in an assertion. Its header names no size (there
    /// is not one in those bytes), so it weighs its bytes — the fallback.
    fn tiny_image() -> Image {
        Image {
            path: "shots/tiny.png".into(),
            mime: "image/png".into(),
            bytes: vec![0xFF, 0xFE],
            pixels: None,
        }
    }
}
