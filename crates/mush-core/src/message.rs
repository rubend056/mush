//! OpenAI-compatible chat message and request/response types.
//!
//! These are intentionally loose (`Option` everywhere, `#[serde(default)]`) so
//! that the many "OpenAI-compatible" servers out there all round-trip cleanly.

use std::borrow::Cow;

use serde::ser::{SerializeSeq, SerializeStruct};
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

/// The `arguments` a call arrived with, as the JSON *text* the spec's own
/// request form spells.
///
/// A well-behaved server sends the text (`"{\"path\":\"a\"}"`); one that
/// already holds the object sends the object instead, and both are the same
/// call. An object is re-spelled as its JSON text rather than refused: mush
/// parses that text back when the call is run, and a strict endpoint is sent
/// the string again. `null` (and an absent field, via `serde(default)`) is no
/// arguments, which is what a call that asks for none means.
fn arguments_from_wire<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(match Option::<Value>::deserialize(deserializer)? {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text,
        Some(other) => other.to_string(),
    })
}

/// The `id` a call arrived with, as text.
///
/// The spec spells a call id as a string; a server that numbered its calls
/// sends a number, and a number is not a malformed reply. Any value that is
/// not a string is spelled as its JSON text — `1` as `"1"` — and
/// [`assign_tool_call_ids`] is the one place that decides what the text means
/// for the batch (a missing or repeated id becomes `call_N`).
fn id_from_wire<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(match Option::<Value>::deserialize(deserializer)? {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text,
        Some(other) => other.to_string(),
    })
}

/// The `tool_calls` wire field, normalized on the way in: a reply is not
/// malformed because a model left an id out, repeated one, numbered one, or
/// left a call's `function`/`name` out (each of those shapes is read where it
/// arrives — [`id_from_wire`], [`arguments_from_wire`] — and the batch's ids
/// are settled here by [`assign_tool_call_ids`]).
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

/// One function a call names: the tool, and the arguments it is asked for.
///
/// Both fields default, for the module's promise: a call that names no tool —
/// or carries no function object at all — is one the tool loop can answer
/// (an empty name is the unknown-tool result the model can correct), where a
/// strict parse would fail the whole reply and end the run instead.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FunctionCall {
    #[serde(default)]
    pub name: String,
    /// The call's arguments as the spec's JSON text, whatever shape they
    /// arrived in (see `arguments_from_wire`).
    #[serde(default, deserialize_with = "arguments_from_wire")]
    pub arguments: String,
}

/// One call in a reply's `tool_calls`: the id a result is paired with, and the
/// function it asks for.
///
/// Every field defaults so that a server which leaves one out — or spells the
/// id as a number (`id_from_wire`) — is read rather than refused: the tool
/// loop answers what it can and tells the model about what it cannot, which is
/// the road a loose reply is supposed to keep open.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    /// The id a tool result is paired with. `serde(default)` covers a missing
    /// one and `id_from_wire` a numeric one; `assign_tool_call_ids` then
    /// makes whatever arrived unique and non-empty.
    #[serde(default, deserialize_with = "id_from_wire")]
    pub id: String,
    /// The call's `type`. Defaulted to the spec's `"function"`: that is the
    /// only kind this tree runs, and a server that omits the field is not
    /// malformed.
    #[serde(rename = "type", default = "function_type")]
    pub kind: String,
    /// The function the call names. Defaulted for the same reason as the name
    /// and arguments inside it: a missing object is an empty call, not a reply
    /// to throw away.
    #[serde(default)]
    pub function: FunctionCall,
}

/// One image carried inside a message: a screenshot, a chart, a rendered
/// diagram the model is being asked to look at.
///
/// The bytes live in the message rather than behind a URL because mush has no
/// server to put them on: a request is the only thing that leaves this
/// machine, so anything the model is to look at has to travel in it. `path` is
/// workspace-relative, the name the producer read it from — the one fact that
/// makes the image findable again once the bytes are gone (the byte cap gives
/// a picture's payload up, a trimmed history and a saved session drop the
/// turn or the bytes). `mime` is what the `data:` URL tells the endpoint the
/// bytes are. `pixels` is what the picture *costs*, which is a different fact
/// from how many bytes it took to write: [`Image::weight`] prices an image by
/// this, so a 70 KB and a 724 KB screenshot of the same size weigh the same.
/// `size` is how many bytes the payload was, which outlives the payload itself
/// ([`Image::give_up_payload`]).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Image {
    pub path: String,
    pub mime: String,
    pub bytes: Vec<u8>,
    /// The payload's size in bytes, as it was when the picture was built.
    ///
    /// `bytes` is where the payload lives while the picture travels, and a
    /// copy that cannot keep the bytes empties it
    /// ([`Image::give_up_payload`]) — so after that `bytes` is no way to say
    /// how big the picture ever was. This is the fact that stays, and the
    /// readers that need it do not measure the buffer: the pane's `▣` row
    /// reads it (the app's `image_label`), so the line says the picture's real
    /// size after the payload is gone. Set by [`Image::new`] — and only there,
    /// `Default`'s empty picture aside: a literal that spelled it by hand
    /// would be a size the picture can change under.
    #[serde(default)]
    size: usize,
    /// The picture's width and height, read from its own header
    /// ([`crate::workspace::image_dimensions`]) when it was read from disk.
    ///
    /// `None` when no size could be read — a truncated file, a format whose
    /// header carries none, bytes that are not what the mime claims — and then
    /// [`Image::weight`] falls back to the payload's own size ([`Image::size`]),
    /// which errs high for a picture: the safe direction, because a transcript
    /// that weighs too little is the request that goes out over the window,
    /// while a picture that weighs too much costs the conversation its oldest
    /// turn.
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
    /// A picture built from the bytes a road read: the payload, and the size
    /// the payload has. The one way an `Image` is built *from bytes* — the
    /// size is taken from them here, so no construction can put the two facts
    /// out of step, and no caller has to remember to keep them so.
    pub fn new(
        path: impl Into<String>,
        mime: impl Into<String>,
        bytes: Vec<u8>,
        pixels: Option<(u32, u32)>,
    ) -> Self {
        Self {
            path: path.into(),
            mime: mime.into(),
            size: bytes.len(),
            bytes,
            pixels,
        }
    }

    /// The payload's size in bytes, from when the picture held them
    /// ([`Self::new`]) — a fact that survives the payload itself.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Give up the payload: the bytes go, everything else stays — the path,
    /// the mime, [`Self::size`] and the pixels. What a copy that cannot keep
    /// the bytes does with a picture — a conversation's byte cap empties the
    /// payload of every picture past it, and a view that will never send one
    /// leaves it behind whole. The buffer is dropped, not cleared, so the
    /// memory goes back with it; the path names the file a reader can read
    /// again.
    pub fn give_up_payload(&mut self) {
        self.bytes = Vec::new();
    }

    /// What this picture costs the context budget, in the byte-shaped currency
    /// [`Message::weight`] counts — its pixels at the
    /// [`PIXELS_PER_TOKEN`](crate::config::PIXELS_PER_TOKEN) rule, or its own
    /// size when no header named one, plus the path and mime that travel
    /// with it.
    ///
    /// Pixels are the estimate because pixels are what a vision endpoint's
    /// price is made of: a 1920×1080 screenshot is ~2.8k tokens whether it is
    /// written as a 724 KB png or a 70 KB one, where its file size alone used
    /// to read as ~247k ([`crate::config::tokens_for_pixels`] turns the
    /// picture into tokens, and [`BYTES_PER_TOKEN`](crate::config::BYTES_PER_TOKEN)
    /// turns them back, so text and pictures stay in one currency).
    ///
    /// The fallback reads the stored size, not the live buffer, so a picture
    /// costs the same whether or not its payload has been given up: the byte
    /// cap is a byte cap, and the window it is weighed against keeps the
    /// picture's price.
    pub fn weight(&self) -> usize {
        let payload = match self.pixels {
            Some((width, height)) => {
                crate::config::tokens_for_pixels(u64::from(width) * u64::from(height))
                    .saturating_mul(crate::config::BYTES_PER_TOKEN)
            }
            // No size to read: the picture's own size is the fallback, and for
            // a picture it overcounts, which is the safe direction (see the
            // field).
            None => self.size,
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

/// How many bytes of picture payload a conversation keeps, besides the newest
/// message's own.
///
/// The unit is bytes because the thing bounded is memory, and a picture's
/// window price is not its size: a 100×100 png weighing 2 MB costs fourteen
/// tokens ([`Image::weight`]), so a hundred of them pass every token bound the
/// window has while the process holds 200 MB of payload. Four of the files the
/// transport already caps one at ([`crate::workspace::IMAGE_FILE_CAP`]) is the
/// working set a pane's scrollback is worth keeping; past it
/// [`retain_image_bytes`] gives a picture's payload up and keeps its path,
/// which is what reads the picture again.
pub const IMAGE_BYTES_KEPT: usize = 8 * 1024 * 1024;

/// Give up the picture payloads a conversation cannot keep: the newest
/// pictures keep theirs while they fit `cap`, and an older picture keeps
/// everything but its bytes.
///
/// The walk goes from the newest message back, and within a message from its
/// newest picture back. A picture keeps its payload while its [`Image::size`]
/// fits what the cap has left, and every picture that does not fit gives its
/// payload up ([`Image::give_up_payload`]) — its path, mime, size and pixels
/// stay. The newest message's own pictures are never given up, whatever they
/// weigh: the box accepts eight of the transport's 2 MiB files in one message,
/// so a paste larger than `cap` must still reach the model whole, and the peak
/// a conversation holds is therefore the newest message plus `cap`.
///
/// This is the one rule for both places a conversation's bytes live: the
/// pane's record, on every line appended to it, and the actor's own history,
/// before it builds a request. A picture whose payload is gone is spelled on
/// the wire as the `placeholder` sentence — never an empty `data:` URL — so
/// the model is told the bytes are gone and which file reads them again.
///
/// The cost, and the reason the value was ruled rather than derived: giving a
/// picture's payload up rewrites what the request says at that message, and an
/// endpoint that cached the request's prefix re-prefills the tail from there.
/// With 2 MiB screenshots this cap keeps about four pictures behind the newest
/// message, so the changed point sits about four pictures back — the ruling's
/// estimate is a re-prefill about every fourth new picture. The shape weighed
/// beside it and set aside is one constant away: keep to a 32 MiB ceiling and
/// shed back down to this cap in one pass, which changes the request about
/// every twelfth new picture for a 32 MiB bound but moves the changed point
/// much further back when it does. The human chose the flat cap.
///
/// It is not a wire bound, and it never keeps a picture out of a request that
/// could carry it: only payloads a conversation has already given up are
/// missing from a request, and a request whose body is past the transport's
/// ceiling is refused with its own line (`MAX_REQUEST_BYTES`, in the agent) —
/// never silently shrunk here.
pub fn retain_image_bytes(messages: &mut [Message], cap: usize) {
    let mut kept = 0usize;
    let newest = messages.len().saturating_sub(1);
    for (index, message) in messages.iter_mut().enumerate().rev() {
        if index == newest {
            continue;
        }
        for picture in message.images.iter_mut().rev() {
            let size = picture.size();
            if size.saturating_add(kept) <= cap {
                kept = kept.saturating_add(size);
            } else {
                picture.give_up_payload();
            }
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Message {
    /// The speaker. `serde(default)`, because a reply that omits it is still a
    /// reply: the reply road does not match on this field — the run pushes the
    /// choice's message as the model's turn and answers whatever it holds — so
    /// a missing role must not be the thing that ends a run. The empty string
    /// is what "the server did not say" reads as.
    #[serde(default)]
    pub role: String,
    /// The message's text. A plain JSON string on the way out — the spec's own
    /// request form, for both assistant history and tool results — unless the
    /// message carries images, when it is the content array
    /// (`Message::content_parts`). On the way in, either wire shape (see
    /// `content_from_wire`).
    #[serde(default, deserialize_with = "content_from_wire")]
    pub content: Option<String>,
    /// Images carried *in* this message, in the order they were attached.
    /// They are not a wire field of their own: they become `image_url` parts
    /// *inside* `content` on the way out (`Message::content_parts`), and a
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
    /// Whether this message is mush's dropped-turns note rather than a line
    /// someone said: the provenance [`crate::transcript::is_dropped_note`]
    /// reads, set only by the constructor this flag is named after.
    ///
    /// The flag exists because the *text* cannot be the shape (finding F3): a
    /// human's message, a parent's brief or a nudge that is word for word
    /// [`DROPPED_TURNS_NOTE`](crate::transcript::DROPPED_TURNS_NOTE) used to be
    /// taken for the note, removed from where it sat and re-inserted at index
    /// 2 — past an assistant turn, or between an assistant's tool call and its
    /// result. The sentence itself stays exactly what it was: it is what the
    /// model is told, and the one caller hands over `DROPPED_TURNS_NOTE` for
    /// it.
    ///
    /// Not a wire field: the request an endpoint reads carries the sentence
    /// and nothing about who wrote it, so [`Message`]'s own serializer never
    /// names this flag. It *is* written to `.mush/session.json` and read back
    /// ([`serialize_stored_messages`], the shape [`crate::session::Session`]
    /// stores its transcripts in), because the one thing a restart cannot
    /// rebuild is this flag: without it a restored transcript reads its note
    /// as a plain `user` line — the trimmer counts it as a turn and the pane
    /// paints it in the human's voice — and its line is no longer moved back
    /// where the dropped turns were. `false` is what every other message reads
    /// as, the wire and an older file included: `serde(default)` keeps the
    /// field a fact about the note alone, so a human's line that is word for
    /// word `DROPPED_TURNS_NOTE` is still the human's own.
    #[serde(default)]
    pub note: bool,
    /// Whether mush itself wrote this line, rather than the human, a parent or
    /// the model: the provenance a pane paints its own voice from (finding
    /// F3), and the one thing about a line that is never read off its words.
    ///
    /// The lines that carry it are mush's words *to* a run — the loop guard's
    /// warning before a resumed run, the instructions a cut-off or unreadable
    /// reply is answered with, and the report a failed commit leaves — every
    /// one of them a `user` message, which is the shape a request reads them
    /// in, and every one of them a line nobody said. The pane used to place
    /// the failed commit's line by its opening words alone and had nothing at
    /// all to place the other three, so a child's pane painted mush's own
    /// instructions as its parent's; the flag decides instead, exactly as
    /// [`Message::note`](Message::note) does for the note.
    ///
    /// Not a wire field: the model reads the sentence alone, so an endpoint
    /// cannot tell a marked line from a human's identical one. It *is* written
    /// to `.mush/session.json` and read back ([`serialize_stored_messages`]),
    /// because a restart has no other road to the fact — without it a restored
    /// line the actor marked would paint as whoever the pane falls back to.
    /// `false` is what every other message reads as, the wire and an older
    /// file included (`serde(default)`), so the flag stays a fact about the
    /// lines that carry it.
    #[serde(default)]
    pub mush: bool,
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
///
/// This is the **wire** form: it is what the request path sends, so it names
/// nothing the spec does not. The two fields that are not the spec's — `note`
/// and `mush` — are written only by [`serialize_stored_messages`], the shape
/// the session file stores.
impl Serialize for Message {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.write(serializer, false)
    }
}

/// One message in the shape `.mush/session.json` stores it: [`Message`]'s wire
/// form, plus `note` and `mush` when those flags are set.
///
/// Why the file must have them and the wire must not is
/// [`Message::note`](Message::note)'s doc; `mush`'s is
/// [`Message::mush`](Message::mush)'s. Why not a session-level fact: each flag
/// belongs to one message, and a boolean on the file would have to be matched
/// back to a line — by prose, the one thing finding F3 forbids — while the
/// message is where the flag already lives.
///
/// Only the `true` is written, for either flag. A message with neither is byte
/// for byte what the wire form writes, so a session file stays readable by a
/// mush that predates the field, and `#[serde(default)]` is what a file that
/// predates it reads back as (`false`).
pub fn serialize_stored_messages<S>(messages: &[Message], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut seq = serializer.serialize_seq(Some(messages.len()))?;
    for message in messages {
        seq.serialize_element(&Stored(message))?;
    }
    seq.end()
}

/// One message in the stored shape [`serialize_stored_messages`] writes: a
/// wrapper, not a second shape — it delegates to the one body below.
struct Stored<'a>(&'a Message);

impl Serialize for Stored<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.write(serializer, true)
    }
}

impl Message {
    /// The message's JSON: the wire form, and — with `stored`, when the flag is
    /// set — the one field the session file adds to it. One body, because two
    /// would be two spellings of the same shape to keep in step.
    fn write<S>(&self, serializer: S, stored: bool) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let note = stored && self.note;
        let mush = stored && self.mush;
        let fields = 1
            + usize::from(self.content.is_some() || !self.images.is_empty())
            + usize::from(self.reasoning_content.is_some())
            + usize::from(self.tool_calls.is_some())
            + usize::from(self.tool_call_id.is_some())
            + usize::from(note)
            + usize::from(mush);
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
        if note {
            message.serialize_field("note", &true)?;
        }
        if mush {
            message.serialize_field("mush", &true)?;
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

    /// The dropped-turns note: the only constructor that sets `note`, so a
    /// line a human typed, a parent briefed or a nudge quoted cannot become
    /// the note by being word for word its sentence (finding F3).
    ///
    /// The text stays the caller's — [`trim_history`](crate::transcript::trim_history)
    /// hands over [`DROPPED_TURNS_NOTE`](crate::transcript::DROPPED_TURNS_NOTE)
    /// — because the sentence is what the model reads, and this constructor
    /// holds no second copy of it. The role is the user's, the voice mush's
    /// other out-of-band notes use (`COMPACT_INSTRUCTION`,
    /// `TRUNCATION_INSTRUCTION`, a folded completion); the flag is what tells
    /// the note from a user line for the two readers that must know
    /// ([`crate::transcript::is_dropped_note`]).
    pub fn note(text: impl Into<String>) -> Self {
        Self {
            note: true,
            ..Self::user(text)
        }
    }

    /// A line mush itself wrote into the conversation: the one constructor
    /// that sets [`Message::mush`], so the mark is a fact about the hand that
    /// wrote a line and never about what the line says (finding F3) — a human
    /// whose message is word for word one of these lines keeps their own voice.
    ///
    /// The text stays the caller's, because the sentence is what the model
    /// reads and this constructor holds no second copy of it. The role is the
    /// user's, the shape a request reads an out-of-band line in and the one
    /// [`Message::note`](Message::note) and the compact instruction already
    /// use; the flag is what tells the pane whose line it is.
    pub fn mush(text: impl Into<String>) -> Self {
        Self {
            mush: true,
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
    /// when there is none), then one part per image — an `image_url` part
    /// holding a `data:` URL, or, for a picture whose payload was given up
    /// ([`Image::give_up_payload`], [`retain_image_bytes`]), the same
    /// `placeholder` sentence [`Message::drop_images`] writes, as a text part.
    /// This is the vision form of the spec, and the only way an image rides in
    /// a request: [`ChatRequest`] knows nothing about it.
    ///
    /// A picture with no payload is never an empty `data:` URL: the endpoint
    /// would read a broken picture, and the model is owed the one fact that
    /// matters — the bytes were dropped to save room, and the path names the
    /// file that reads them again.
    fn content_parts(&self) -> Vec<ContentPart<'_>> {
        let mut parts = Vec::with_capacity(self.images.len() + 1);
        if !self.text().is_empty() {
            parts.push(ContentPart {
                kind: "text",
                text: Some(Cow::Borrowed(self.text())),
                url: None,
            });
        }
        parts.extend(self.images.iter().map(|image| {
            if image.bytes.is_empty() {
                ContentPart {
                    kind: "text",
                    text: Some(Cow::Owned(placeholder(image))),
                    url: None,
                }
            } else {
                ContentPart {
                    kind: "image_url",
                    text: None,
                    url: Some(data_url(image)),
                }
            }
        }));
        parts
    }

    /// Shed this message's image payloads, leaving one `placeholder` line
    /// where each was, so the transcript still says an image was there and
    /// which file it came from — and the model can read that file again if it
    /// needs the image.
    ///
    /// The one way an image leaves a live message, and the session writer's
    /// alone: [`Session::save`](crate::session::Session::save) calls it before
    /// it writes `.mush/session.json`, because a screenshot is megabytes no
    /// human wants to find in a file, and the path in the placeholder is what a
    /// resumed run needs to read the picture again. Trimming used to call it
    /// too, shedding payloads before it dropped a turn — a workaround for the
    /// byte-priced image, which read a 700 KB screenshot as 247k tokens and
    /// made a picture the first thing to go. With images priced by their pixels
    /// ([`Image::pixels`], [`PIXELS_PER_TOKEN`](crate::config::PIXELS_PER_TOKEN))
    /// a picture is a normal-sized part of the turn it arrived in, and the turn
    /// is what a trim drops: images and words together.
    ///
    /// Idempotent on purpose: a message that already lost its images has
    /// nothing left to shed, so re-saving a loaded session cannot stack a
    /// second placeholder on the first one's text.
    ///
    /// A payload can leave a live message without the image: the byte cap
    /// ([`retain_image_bytes`]) gives it up and the entry stays — the pane's
    /// `▣` row still names the picture and reads its [`Image::size`] — where
    /// the wire spells it as this same sentence (`Message::content_parts`).
    /// This method is what removes the entry itself.
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
    /// own stored size when its header named no size — plus the path and mime
    /// that travel with it. Base64's 4/3 inflation is deliberately *not* modeled:
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
    /// The part's text: borrowed from the message, or owned when it is a
    /// `placeholder` sentence built for a picture whose payload is gone.
    text: Option<Cow<'a, str>>,
    url: Option<String>,
}

impl Serialize for ContentPart<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut part = serializer.serialize_struct("content_part", 2)?;
        part.serialize_field("type", self.kind)?;
        if let Some(text) = &self.text {
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

/// The line a dropped image leaves behind, in the one spelling the session
/// writer uses (never two). It names the path, because that is what makes the
/// image reachable again — the model can read the file — and the format, so
/// the line cannot be mistaken for something the model said. The path goes
/// through [`one_line`], so the placeholder is exactly one line whatever the
/// path holds.
fn placeholder(image: &Image) -> String {
    // `image/png` prints as `png`: the mime already leads with the fact that
    // this is an image, and the sentence has room for one noun.
    let format = image.mime.strip_prefix("image/").unwrap_or(&image.mime);
    let name = one_line(&image.path);
    if name == image.path {
        format!(
            "[image: {name} ({format}) — bytes dropped to save room; read the file again if you \
             need them]"
        )
    } else {
        // The spelling above is a mark, not the name: a break in the name, an
        // escape sequence inside it, a byte the painter drops — the model
        // cannot pass the shown line back to `read_file` and reach the file,
        // so the placeholder must not say it can (finding B9). The shell is
        // the road that still reaches it.
        format!(
            "[image: {name} ({format}; the name cannot travel as a path) — bytes dropped to save \
             room; run_command (`ls -b`) reaches the file if you need it]"
        )
    }
}

/// The one-line spelling of a path that goes into a line which must stay one
/// line.
///
/// [`crate::text::sanitize`] is what this repo paints untrusted text through:
/// it removes escape sequences and control characters whole, so the path cannot
/// command the pane it is shown in. It deliberately keeps `\n` — the wrapper
/// splits on it — and a placeholder is not wrapped but shaped: a newline in a
/// path (legal on Linux) would otherwise put a line of its own into the model's
/// view of the transcript, where the placeholder is a stand-in for bytes and
/// not a message of its own. The break is spelled as the two characters `\n`
/// rather than dropped, and a lone `\r` [`crate::text::sanitize`] already marks
/// as `␍`, which is one line as well.
///
/// That spelling is a *mark*, not the name: it is not a path `read_file` can
/// open, and no spelling of one can be, because a listing's line and a tool
/// argument are one line by construction. So [`placeholder`] says so and names
/// the shell road instead of inviting a read that would fail (finding B9).
fn one_line(path: &str) -> String {
    crate::text::sanitize(path).replace('\n', "\\n")
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

    /// The module's opening promise, as the four shapes that broke it: each
    /// names a field a server left out or spelled its own way, and each used
    /// to fail the *whole* reply — the `ChatResponse` parse whose `Err` becomes
    /// "could not parse model response" and ends the run. A loose reply is
    /// still a reply: the message parses, an object `arguments` arrives as its
    /// JSON text, a numeric id as its text, and a choice with no `role` is the
    /// model's own turn like any other.
    #[test]
    fn a_loose_reply_is_still_a_reply() {
        // No `role`: the field defaults, and the text is the reply.
        let roleless: Message = serde_json::from_str(r#"{"content":"hi"}"#).unwrap();
        assert_eq!(
            roleless.role, "",
            "a missing role is an empty one, not a refused reply"
        );
        assert_eq!(roleless.text(), "hi");

        // No `function` object at all: an empty function, which the tool loop
        // answers as the unknown-tool road rather than dropping the reply.
        let bare_call: Message = serde_json::from_str(
            r#"{"role":"assistant","content":[{"type":"text","text":"hi"}],"tool_calls":[{"id":"x","type":"function"}]}"#,
        )
        .unwrap();
        assert_eq!(bare_call.text(), "hi");
        assert_eq!(bare_call.tool_calls().len(), 1);
        assert_eq!(bare_call.tool_calls()[0].function.name, "");
        assert_eq!(bare_call.tool_calls()[0].function.arguments, "");

        // A `function` present but with no `name` is the same empty call.
        let nameless: Message = serde_json::from_str(
            r#"{"role":"assistant","tool_calls":[{"id":"x","type":"function","function":{}}]}"#,
        )
        .unwrap();
        assert_eq!(nameless.tool_calls()[0].function.name, "");

        // `arguments` as an object: it arrives as its JSON text, which is what
        // a strict endpoint is sent back.
        let object_args: Message = serde_json::from_str(
            r#"{"role":"assistant","content":"hi","tool_calls":[{"id":"x","type":"function","function":{"name":"read_file","arguments":{"path":"a"}}}]}"#,
        )
        .unwrap();
        assert_eq!(
            object_args.tool_calls()[0].function.arguments,
            r#"{"path":"a"}"#,
            "an object arrives as the JSON text the spec calls for"
        );
        assert_eq!(
            serde_json::to_string(&object_args).unwrap(),
            r#"{"role":"assistant","content":"hi","tool_calls":[{"id":"x","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a\"}"}}]}"#,
            "and goes back out as the string a strict endpoint expects"
        );

        // A numeric id: it arrives as its text, and `assign_tool_call_ids`
        // leaves a unique non-empty one alone.
        let numeric_id: Message = serde_json::from_str(
            r#"{"role":"assistant","content":"hi","tool_calls":[{"id":1,"type":"function","function":{"name":"read_file","arguments":"{}"}}]}"#,
        )
        .unwrap();
        assert_eq!(numeric_id.tool_calls()[0].id, "1");

        // And the whole reply the run parses, choice and all: a server that
        // sends no `role` is read here, not refused.
        let reply: ChatResponse = serde_json::from_str(
            r#"{"choices":[{"message":{"content":"hi"},"finish_reason":"stop"}]}"#,
        )
        .unwrap();
        assert_eq!(reply.choices[0].message.text(), "hi");
        assert_eq!(
            reply.choices[0].message.role, "",
            "a role the server did not send is not an ended run"
        );
    }

    /// The note's flag is mush's own provenance, not a fact about the request:
    /// what an endpoint reads is the sentence alone, exactly as a human's
    /// identical line reads. The session file's shape is the one that carries
    /// it ([`serialize_stored_messages`]), because a restart has no other road
    /// back to the fact of which line is the note (finding F3).
    #[test]
    fn the_notes_provenance_stays_off_the_wire_and_travels_in_the_file() {
        let sentence = "the oldest turns were dropped";
        let note = Message::note(sentence);
        assert_eq!(
            serde_json::to_string(&note).unwrap(),
            serde_json::to_string(&Message::user(sentence)).unwrap(),
            "a request cannot tell the note from a human's identical line"
        );

        let mut bytes = Vec::new();
        let mut serializer = serde_json::Serializer::new(&mut bytes);
        serialize_stored_messages(&[note], &mut serializer).unwrap();
        let stored = String::from_utf8(bytes).unwrap();
        assert!(
            stored.contains(r#""note":true"#),
            "the file says which line is the note: {stored}"
        );
        assert!(
            !stored.starts_with(r#"{"note""#),
            "and stays a message: {stored}"
        );
    }

    /// The `mush` flag is the same kind of provenance as the note's, for the
    /// lines mush writes *to* a run: off the wire, so the model reads the
    /// sentence alone, and on the session file, so a pane repaints the line as
    /// mush's after a restart instead of falling back to whoever the transcript
    /// cannot place (finding F3).
    #[test]
    fn a_mush_lines_provenance_stays_off_the_wire_and_travels_in_the_file() {
        let sentence = "Your previous run was stopped as a loop: the same tool call repeated";
        let marked = Message::mush(sentence);
        assert_eq!(
            serde_json::to_string(&marked).unwrap(),
            serde_json::to_string(&Message::user(sentence)).unwrap(),
            "a request cannot tell mush's line from a human's identical one"
        );

        let mut bytes = Vec::new();
        let mut serializer = serde_json::Serializer::new(&mut bytes);
        serialize_stored_messages(&[marked], &mut serializer).unwrap();
        let stored = String::from_utf8(bytes).unwrap();
        assert!(
            stored.contains(r#""mush":true"#),
            "the file says which line is mush's: {stored}"
        );
        assert!(
            !stored.starts_with(r#"{"mush""#),
            "and stays a message: {stored}"
        );
    }

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

    /// The placeholder is the one spelling a saved session leaves: it names the
    /// path (so the model can read the file again) and the format, and dropping
    /// twice adds nothing — the idempotence a second save of a loaded session
    /// depends on.
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

    /// The placeholder weighs its own text and nothing else: once a payload is
    /// shed, neither its pixels nor its bytes are in the message's weight any
    /// more. The session writer is the one caller of [`Message::drop_images`],
    /// and this is why a saved — or reloaded — conversation is the size of its
    /// words.
    #[test]
    fn a_shed_payload_leaves_only_the_placeholders_weight() {
        let mut message = Message::assistant("here it is");
        message.images.push(Image::new(
            "shots/screen.png",
            "image/png",
            vec![0x41; 741_396],
            Some((1_920, 1_080)),
        ));
        let with_image = message.weight();

        message.drop_images();

        let text = message.text().to_string();
        assert_eq!(
            message.weight(),
            "assistant".len() + text.len(),
            "role and placeholder text, and no payload: {text}"
        );
        assert!(message.weight() < with_image, "the picture is gone");
        assert!(
            with_image - message.weight() > 8_000,
            "and its ~8.3 KB of weight with it: {with_image} -> {}",
            message.weight()
        );
        assert!(
            text.contains("shots/screen.png"),
            "and the path that replaces it is there: {text}"
        );
    }

    /// The fallback: an image whose header named no size is the bytes it took
    /// to write, plus the path and mime that travel with them — which
    /// overcounts a picture, the safe direction. A transcript that carried
    /// such an image is not the size of its text, and a trimmer that thought
    /// it was would never drop the turn that actually exceeds the window.
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
        message.images.push(Image::new(
            "shots/screen.png",
            "image/png",
            vec![0x41; 741_396],
            Some((1_920, 1_080)),
        ));

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
        let image = Image::new("shots/cut.png", "image/png", vec![0x41; 500_000], None);
        assert_eq!(
            image.weight(),
            image.bytes.len() + image.path.len() + image.mime.len(),
            "unknown pixels fall back to the bytes, and the fallback errs high"
        );
        // The pixels rule would have said ~2k tokens; the bytes say ~166k.
        assert!(image.weight() / BYTES_PER_TOKEN > 160_000);
    }

    /// The size is a fact about the picture, not a measurement of the buffer it
    /// happens to be holding: when a copy gives the payload up
    /// ([`Image::give_up_payload`]) the size stays — and so does the price,
    /// because the byte cap is a cap on bytes and not on the tokens the window
    /// is stated in: [`Image::weight`]'s fallback reads the stored size, never
    /// the emptied buffer.
    #[test]
    fn a_picture_keeps_its_size_and_its_price_when_its_payload_is_given_up() {
        let mut headerless = Image::new("shots/cut.png", "image/png", vec![0x41; 500_000], None);
        let mut pictured = Image::new(
            "shots/screen.png",
            "image/png",
            vec![0x41; 741_396],
            Some((1_920, 1_080)),
        );
        let prices = [headerless.weight(), pictured.weight()];

        for image in [&mut headerless, &mut pictured] {
            image.give_up_payload();
            assert!(image.bytes.is_empty(), "the payload is gone");
        }
        assert_eq!(headerless.size(), 500_000, "the size is not");
        assert_eq!(pictured.size(), 741_396);
        assert_eq!(
            [headerless.weight(), pictured.weight()],
            prices,
            "and the price is not: the cap is bytes, the window is tokens"
        );
    }

    /// Every payload in a transcript, for the cap's tests: what a copy that has
    /// not given anything up yet is holding.
    fn payload_bytes(messages: &[Message]) -> usize {
        messages
            .iter()
            .flat_map(|message| &message.images)
            .map(|image| image.bytes.len())
            .sum()
    }

    /// A picture of `bytes` bytes in its own message, for the cap's tests.
    fn paste(path: &str, bytes: usize) -> Message {
        Message::user_with_images(
            format!("paste {path}"),
            vec![Image::new(path, "image/png", vec![0x41; bytes], None)],
        )
    }

    /// The cap's walk, one 2 MiB picture per message: the newest message's own
    /// is on top of the cap, and the pictures behind it are kept newest-first
    /// while they fit. Five payloads survive — the newest plus the four the cap
    /// holds — and the sixth and oldest gives its payload up with its facts
    /// intact.
    #[test]
    fn the_cap_keeps_the_newest_pictures_and_gives_up_the_older_ones() {
        const PICTURE: usize = 2 * 1024 * 1024;
        let mut messages: Vec<Message> = (0..6)
            .map(|i| paste(&format!("shots/{i}.png"), PICTURE))
            .collect();

        retain_image_bytes(&mut messages, IMAGE_BYTES_KEPT);

        assert_eq!(
            payload_bytes(&messages),
            IMAGE_BYTES_KEPT + PICTURE,
            "the newest message's own picture plus the cap's worth"
        );
        assert!(
            messages[0].images[0].bytes.is_empty(),
            "the oldest payload is gone"
        );
        assert_eq!(messages[0].images[0].size(), PICTURE, "its size is not");
        assert_eq!(messages[0].images[0].path, "shots/0.png", "nor is its path");
        for message in &messages[1..] {
            assert!(
                !message.images[0].bytes.is_empty(),
                "the newer payloads stay: {message:?}"
            );
        }
    }

    /// The newest message's own pictures are never given up, whatever they
    /// weigh: the box accepts 8 × 2 MiB in one message, and a paste larger than
    /// the cap still reaches the model whole.
    #[test]
    fn the_newest_messages_pictures_are_never_given_up() {
        let pictures: Vec<Image> = (0..3)
            .map(|i| {
                Image::new(
                    format!("shots/{i}.png"),
                    "image/png",
                    vec![0x41; 4 * 1024 * 1024],
                    None,
                )
            })
            .collect();
        let mut messages = vec![Message::user_with_images("look", pictures)];

        retain_image_bytes(&mut messages, IMAGE_BYTES_KEPT);

        assert_eq!(
            payload_bytes(&messages),
            12 * 1024 * 1024,
            "12 MiB in one message, past the cap and whole"
        );
    }

    /// The walk is per picture, not per message: a message that straddles the
    /// cap keeps the newest of its own pictures and gives up the older ones.
    #[test]
    fn a_message_that_straddles_the_cap_keeps_its_newest_pictures() {
        const PICTURE: usize = 4 * 1024 * 1024;
        let straddling = Message::user_with_images(
            "three wide",
            (0..3)
                .map(|i| {
                    Image::new(
                        format!("shots/{i}.png"),
                        "image/png",
                        vec![0x41; PICTURE],
                        None,
                    )
                })
                .collect(),
        );
        let mut messages = vec![
            Message::user("older words"),
            straddling,
            paste("shots/newest.png", 1_024),
        ];

        retain_image_bytes(&mut messages, IMAGE_BYTES_KEPT);

        let kept: Vec<usize> = messages[1]
            .images
            .iter()
            .map(|image| image.bytes.len())
            .collect();
        assert_eq!(
            kept,
            vec![0, PICTURE, PICTURE],
            "the newest two of its pictures fit; the oldest gives up"
        );
    }

    /// What the model sees for a picture past the cap is the sentence the tree
    /// already writes when bytes cannot travel — `Message::drop_images`'s own
    /// line, word for word — as a text part, and never an empty `data:` URL.
    /// The path in it is what reads the file again.
    #[test]
    fn a_picture_past_the_cap_reads_as_the_placeholder_on_the_wire() {
        let mut messages = vec![
            paste("shots/big.png", 2 * 1024 * 1024),
            paste("shots/beside.png", 2 * 1024 * 1024),
        ];
        // A cap smaller than the older picture: the newest message's own stays,
        // and the older one gives its payload up.
        retain_image_bytes(&mut messages, 1_024);
        assert!(messages[0].images[0].bytes.is_empty());

        let wire = serde_json::to_string(&messages[0]).unwrap();
        let mut shed = messages[0].clone();
        shed.drop_images();
        let sentence = shed
            .text()
            .lines()
            .nth(1)
            .expect("the line drop_images writes")
            .to_string();
        assert!(
            wire.contains(&sentence),
            "one wording, part for part: {wire}"
        );
        assert!(
            sentence.contains("shots/big.png"),
            "it names the file: {sentence}"
        );
        assert!(!wire.contains("data:"), "no empty data URL: {wire}");
        assert!(!wire.contains("image_url"), "and no image part: {wire}");
    }

    /// A header can claim the largest size a pair of `u32`s can hold, and the
    /// accounting must not wrap around it: the pixel count is taken in `u64`,
    /// the product and the sums in saturating `usize`, so the picture weighs
    /// "as much as there is" rather than a small number come around again.
    /// The expectation is computed in `u128`, so the test does not repeat the
    /// arithmetic it is checking.
    #[test]
    fn the_largest_pixels_a_header_can_claim_do_not_overflow_the_weight() {
        let image = Image::new(
            "shots/max.png",
            "image/png",
            Vec::new(),
            Some((u32::MAX, u32::MAX)),
        );
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

    /// The placeholder is one line whatever the path holds. A newline is legal
    /// in a Linux path, and before the path was sanitized it put a line of its
    /// own into the transcript — the placeholder is a stand-in for bytes, and a
    /// line is the whole of its shape. The break is spelled `\n`, and because
    /// that spelling is a mark and not the name, the placeholder does not
    /// invite a read that would fail: the shell is the road to a name the tools
    /// cannot carry (finding B9).
    #[test]
    fn a_dropped_image_whose_path_holds_a_newline_still_leaves_one_line() {
        let mut message = Message::user("look");
        message.images.push(Image::new(
            "shots/a\nb.png",
            "image/png",
            vec![0xFF, 0xFE],
            None,
        ));
        message.drop_images();
        let text = message.text().to_string();
        assert_eq!(
            text.lines().count(),
            2,
            "the message's own line, then one placeholder line: {text:?}"
        );
        assert!(
            text.contains(r"[image: shots/a\nb.png (png; the name cannot travel as a path)"),
            "the escaped name is marked as a mark, not a path: {text:?}"
        );
        assert!(
            !text.contains("read the file again"),
            "the model is not sent to a path that does not exist: {text:?}"
        );
        assert!(
            text.contains("run_command (`ls -b`)"),
            "the road that reaches it is named: {text:?}"
        );
    }

    /// The path goes through the repo's sanitizer, so an escape sequence in a
    /// name cannot command the pane it is painted in from inside a line that
    /// says an image was dropped. What is not a command stays, so the line
    /// still points at the file.
    #[test]
    fn a_dropped_image_whose_path_holds_an_escape_sequence_leaves_no_command() {
        let mut message = Message::user("");
        message.images.push(Image::new(
            "a\x1b]0;PWNED\x07b.png",
            "image/png",
            vec![0xFF, 0xFE],
            None,
        ));
        message.drop_images();
        let text = message.text().to_string();
        assert!(
            !text.contains('\x1b') && !text.contains('\x07'),
            "the sequence is removed whole: {text:?}"
        );
        assert!(
            text.contains("ab.png"),
            "what is not a command stays: {text:?}"
        );
    }

    /// The one image the image tests are about: two bytes, a path and a mime,
    /// small enough to read in an assertion. Its header names no size (there
    /// is not one in those bytes), so it weighs its bytes — the fallback.
    fn tiny_image() -> Image {
        Image::new("shots/tiny.png", "image/png", vec![0xFF, 0xFE], None)
    }
}
