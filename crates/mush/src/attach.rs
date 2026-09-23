//! The attach socket: an external agent drives a running mush (M3).
//!
//! A UNIX socket lives at `<root>/.mush/mush.sock` for as long as mush runs.
//! The listener has a thread of its own, and each connection another, so a
//! client that goes quiet cannot block the ones behind it (finding A2); none of
//! those threads ever touches `App`'s state. A request line is parsed, handed
//! to the UI thread as [`Msg::Attach`] with a one-shot reply channel, and the
//! answer is written back. `App` stays the only effector.
//!
//! Every road in — the line, the connections, the time a client may say
//! nothing — is bounded (see [`MAX_REQUEST_BYTES`], [`MAX_CONNECTIONS`] and
//! [`IDLE_TIMEOUT`]), because a surface a same-user process can reach is a
//! surface that must not be able to spend mush's heap, threads or patience.
//!
//! The protocol is newline-delimited JSON: one request and one response per
//! line. Every request carries an `id` (any JSON value, echoed verbatim), and
//! every response carries the same `id` plus either `"ok": {…}` or
//! `"error": {"kind": …, …}`. An edit carries a base revision and is answered
//! with `conflict` rather than guessing.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{bounded, Sender};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app::Msg;

/// The socket file, under `<root>/.mush/`.
const SOCKET_FILE: &str = "mush.sock";

/// How long the accept loop backs off after a failed `accept`, so a listener
/// that has gone bad cannot spin the thread hot.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(20);

/// How long the CLI waits for mush to answer a request, on the socket and on
/// the write. A mush that is alive but not answering must not hang a script
/// (the client half of finding A2).
const ASK_TIMEOUT: Duration = Duration::from_secs(30);

/// How much of a request line the socket will hold before it refuses it.
///
/// 64 KiB is generous for a JSON line, and it is the same decision every other
/// input road in the tree already makes (the clipboard's `READ_CAP`, the HTTP
/// body's `MAX_BODY_BYTES`): a buffer whose size the client does not choose.
/// A line past the cap is answered `bad_request`, naming the cap, and the rest
/// of the line is read and thrown away — constant memory however long the
/// client keeps talking, and the connection's next line is a fresh request.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// How many connections the surface serves at once.
///
/// Each one owns a thread that waits on its client, so without a cap a script
/// that leaks sockets — or a same-user process that means to — spends the
/// process's thread limit. Past the cap a connection is answered `unavailable`
/// and closed: the client can retry, and the surface does not grow.
const MAX_CONNECTIONS: usize = 64;

/// How long a connection may say nothing before it is reaped.
///
/// The protocol is one request and one answer per line, so a client that has
/// connected and then said nothing is either gone or not a client. This is the
/// server's half of the bound [`ask`] already puts on itself ([`ASK_TIMEOUT`]),
/// and the two numbers are the same on purpose.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// What a serve is allowed: the live-connection cap and the idle window. One
/// value rather than two arguments, so the tests can hold a clock and a count
/// while the production numbers stay in one place ([`LIMITS`]).
#[derive(Clone, Copy)]
struct Limits {
    connections: usize,
    idle: Duration,
}

/// The bounds a running mush serves under.
const LIMITS: Limits = Limits {
    connections: MAX_CONNECTIONS,
    idle: IDLE_TIMEOUT,
};

/// Where a running mush listens, and the CLI looks.
pub fn socket_path(root: &Path) -> PathBuf {
    mush_core::session::mushroom_dir(root).join(SOCKET_FILE)
}

/// Owns the socket file for the life of the process; dropping it removes the
/// file, so a clean quit leaves nothing behind.
pub struct Guard {
    path: PathBuf,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Bind the socket and serve it on a thread of its own.
///
/// A bind that fails is never fatal — mush runs without attach and says so on
/// stderr — so the error is returned rather than raised. A socket *file* with
/// nothing listening behind it is a crash's leftover, not a live mush, and is
/// cleared so this bind can take the name; a file a live listener holds is
/// another mush, and the bind refuses the name rather than stealing its
/// socket.
pub fn serve(root: &Path, ui_tx: Sender<Msg>) -> Result<Guard, String> {
    serve_with(root, ui_tx, LIMITS, spawn_accept_loop)
}

/// The accept loop's thread, named the way the rest of the tree's threads are,
/// so a spawn failure is a returned error and not a raised panic.
fn spawn_accept_loop(
    listener: UnixListener,
    ui_tx: Sender<Msg>,
    limits: Limits,
) -> Result<(), String> {
    thread::Builder::new()
        .name("mush-attach".to_string())
        .spawn(move || accept_loop(listener, ui_tx, limits))
        .map(|_| ())
        .map_err(|error| format!("could not start the attach thread: {error}"))
}

/// [`serve`] with the limits a test holds and the one step no test can make
/// fail injected: starting the thread that runs the accept loop. A spawn the OS
/// refuses is exactly what finding A7 is about — the socket file must go with
/// the failed serve — and a thread the OS will not give cannot be asked for on
/// purpose, so the failing start is the test's own.
fn serve_with(
    root: &Path,
    ui_tx: Sender<Msg>,
    limits: Limits,
    start: impl FnOnce(UnixListener, Sender<Msg>, Limits) -> Result<(), String>,
) -> Result<Guard, String> {
    let path = socket_path(root);
    if path.exists() && UnixStream::connect(&path).is_err() {
        let _ = std::fs::remove_file(&path);
    }
    let listener = UnixListener::bind(&path)
        .map_err(|error| format!("could not bind {}: {error}", path.display()))?;
    let guard = Guard { path };
    // The guard is built before the thread so the file is never left behind if
    // the spawn fails: returning here drops it, and its `Drop` removes the
    // socket (finding A7).
    start(listener, ui_tx, limits)?;
    Ok(guard)
}

/// Accept connections, one thread per client, until the listener dies.
///
/// The thread count is bounded by [`Limits::connections`]: a connection past
/// the cap is answered `unavailable` and closed without a thread of its own,
/// and a served connection holds one of the cap's slots until it goes away.
/// The slot is taken here, on the accepting thread, so the count cannot race —
/// the only other change is a connection thread giving its slot back.
fn accept_loop(listener: UnixListener, ui_tx: Sender<Msg>, limits: Limits) {
    let live = Arc::new(AtomicUsize::new(0));
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if live.load(Ordering::SeqCst) >= limits.connections {
                    refuse_busy(stream, limits.connections);
                    continue;
                }
                live.fetch_add(1, Ordering::SeqCst);
                let ui_tx = ui_tx.clone();
                let slot = live.clone();
                // A thread that cannot start ends this connection only; the
                // listener keeps accepting, because a busy box must not take
                // the attach surface down with it. The slot it took goes back
                // at once: the guard that would have given it back on the way
                // out of `serve_connection` never ran.
                let started = thread::Builder::new()
                    .name("mush-attach-conn".to_string())
                    .spawn(move || {
                        let _slot = Live(slot);
                        serve_connection(stream, &ui_tx, limits.idle);
                    });
                if started.is_err() {
                    live.fetch_sub(1, Ordering::SeqCst);
                }
            }
            Err(_) => thread::sleep(ACCEPT_BACKOFF),
        }
    }
}

/// Gives a connection's slot back when its thread ends, however it ends: the
/// guard lives on the connection thread's stack, so a return and a panic both
/// free it — otherwise the surface would refuse clients nothing was holding.
struct Live(Arc<AtomicUsize>);

impl Drop for Live {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Answer a client the surface has no room for, and close.
///
/// The request line is not read: reading it would take the thread the cap just
/// refused, and a client past the cap is owed a retryable answer, not a parse of
/// its request. [`ask`] reads an `unavailable` whose `id` is `null`, which is
/// the only id a request that was never read can carry.
fn refuse_busy(mut stream: UnixStream, max: usize) {
    let response = Response::error(
        Value::Null,
        ReplyError::unavailable(format!(
            "the attach surface already holds {max} connections — retry when one is free"
        )),
    );
    let mut line = response.encode();
    line.push('\n');
    let _ = stream.write_all(line.as_bytes());
    let _ = stream.flush();
}

/// One line from a client, read under [`MAX_REQUEST_BYTES`].
#[derive(Debug)]
enum Line {
    /// A line, newline stripped (or the last bytes before EOF), within the cap.
    Text(String),
    /// A line longer than the cap. It was read to its newline and thrown away,
    /// so the connection stands at the start of its next line.
    Oversize,
    /// The bytes were not UTF-8, so they are not JSON either.
    NotText,
    /// The client closed, or said nothing for the idle window.
    Gone,
}

/// Read one line, holding at most `cap` bytes of it.
///
/// [`BufRead::read_line`] grows with the client, so a line with no newline in
/// it is a heap the client chooses the size of (finding B11). This reads
/// through the reader's own buffer: it keeps at most `cap` bytes and, once a
/// byte past the cap has arrived, stops keeping and drains to the newline
/// instead — constant memory whichever way the line ends. A read that times out
/// is [`Line::Gone`], the same as EOF: the client may come back, but this
/// connection is not going to wait for it.
fn read_line_capped(reader: &mut impl BufRead, cap: usize) -> std::io::Result<Line> {
    let mut line: Vec<u8> = Vec::new();
    let mut oversize = false;
    loop {
        let chunk = match reader.fill_buf() {
            Ok(chunk) => chunk,
            // The idle window fired (SO_RCVTIMEO answers EAGAIN): the client is
            // not saying anything, and this thread has other clients to make
            // room for.
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Ok(Line::Gone)
            }
            Err(error) => return Err(error),
        };
        if chunk.is_empty() {
            // EOF. A line with bytes in it is still a line; nothing at all is
            // the client's end.
            if line.is_empty() && !oversize {
                return Ok(Line::Gone);
            }
            return Ok(finish(line, oversize));
        }
        let newline = chunk.iter().position(|&byte| byte == b'\n');
        let piece = match newline {
            Some(at) => &chunk[..at],
            None => chunk,
        };
        let keep = if oversize {
            false
        } else if line.len() + piece.len() > cap {
            // Keep nothing more: what was refused will not be parsed, and the
            // bytes after the newline are a fresh request.
            oversize = true;
            line.clear();
            false
        } else {
            true
        };
        let piece_len = piece.len();
        if keep {
            line.extend_from_slice(piece);
        }
        let took_newline = newline.is_some();
        reader.consume(piece_len + usize::from(took_newline));
        if took_newline {
            return Ok(finish(line, oversize));
        }
    }
}

/// What the bytes of a finished line are: a line, a line too long, or not text
/// at all.
fn finish(bytes: Vec<u8>, oversize: bool) -> Line {
    if oversize {
        return Line::Oversize;
    }
    match String::from_utf8(bytes) {
        Ok(text) => Line::Text(text),
        Err(_) => Line::NotText,
    }
}

/// Read request lines from one client and answer each, until it goes away. A
/// bad line is answered with an error and does not end the connection; a line
/// past [`MAX_REQUEST_BYTES`] is one of those bad lines, and the connection
/// lives on to answer whatever line comes after it.
///
/// The reads are bounded by the client's own words: a line within the cap, and
/// something said inside the idle window. Both are the socket's, so the waiting
/// is the kernel's and this thread is not woken to check a clock.
fn serve_connection(stream: UnixStream, ui_tx: &Sender<Msg>, idle: Duration) {
    let from = peer_label(&stream);
    if stream.set_read_timeout(Some(idle)).is_err() {
        return;
    }
    let Ok(read_side) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_side);
    let mut writer = stream;
    loop {
        let response = match read_line_capped(&mut reader, MAX_REQUEST_BYTES) {
            Ok(Line::Text(line)) if line.trim().is_empty() => continue,
            Ok(Line::Text(line)) => dispatch(line.trim_end(), &from, ui_tx),
            Ok(Line::Oversize) => Response::error(
                Value::Null,
                ReplyError::bad_request(format!(
                    "the request line is longer than {MAX_REQUEST_BYTES} bytes"
                )),
            ),
            Ok(Line::NotText) => Response::error(
                Value::Null,
                ReplyError::bad_request("the request line is not UTF-8 text"),
            ),
            Ok(Line::Gone) => return,
            Err(_) => return,
        };
        let mut out = response.encode();
        out.push('\n');
        if writer.write_all(out.as_bytes()).is_err() || writer.flush().is_err() {
            return;
        }
    }
}

/// Turn one line into a response: parse it, hand the request to the UI thread,
/// and wait for the answer. A request that will not parse is answered here,
/// without waking `App`.
fn dispatch(line: &str, from: &str, ui_tx: &Sender<Msg>) -> Response {
    let request = match parse_request(line) {
        Ok(request) => request,
        Err(bad) => return Response::error(bad.id, ReplyError::bad_request(bad.message)),
    };
    let id = request.id.clone();
    let (reply_tx, reply_rx) = bounded(1);
    let sent = ui_tx.send(Msg::Attach {
        from: from.to_string(),
        request,
        reply: reply_tx,
    });
    if sent.is_err() {
        return Response::error(id, ReplyError::unavailable("mush is shutting down"));
    }
    match reply_rx.recv() {
        Ok(response) => response,
        Err(_) => Response::error(id, ReplyError::unavailable("the UI dropped the request")),
    }
}

/// How a connection names itself for the bar's line, or `a client` when the
/// peer is unnamed (the usual case for a client that did not bind a path).
fn peer_label(stream: &UnixStream) -> String {
    stream
        .peer_addr()
        .ok()
        .and_then(|addr| addr.as_pathname().map(|path| path.display().to_string()))
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| "a client".to_string())
}

/// The CLI's half of the protocol: connect to the socket under `dir`, send one
/// request, read the one answer. `no mush is running in <dir>` when nothing is
/// bound there, so the caller's message names the directory the human gave.
pub fn ask(dir: &Path, request: &Request) -> Result<Response, String> {
    let socket = socket_path(dir);
    let stream = UnixStream::connect(&socket)
        .map_err(|_| format!("no mush is running in {}", dir.display()))?;
    // The CLI's own bound: a mush that accepts the connection and then stops
    // answering must not hang a script forever (the client half of finding A2).
    stream
        .set_read_timeout(Some(ASK_TIMEOUT))
        .map_err(|error| format!("could not use the socket: {error}"))?;
    stream
        .set_write_timeout(Some(ASK_TIMEOUT))
        .map_err(|error| format!("could not use the socket: {error}"))?;
    let mut writer = stream
        .try_clone()
        .map_err(|error| format!("could not use the socket: {error}"))?;
    let mut bytes = request.encode();
    bytes.push('\n');
    writer
        .write_all(bytes.as_bytes())
        .and_then(|()| writer.flush())
        .map_err(|error| format!("could not send the request: {error}"))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|error| format!("no answer from mush: {error}"))?;
    if line.trim().is_empty() {
        return Err("mush closed the connection without an answer".to_string());
    }
    decode(line.trim_end())
}

// ------------------------------------------------------------------- protocol

/// One parsed request line.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// The `id` the response must echo. Any JSON value; `null` when the request
    /// carried none, or when even the id could not be read.
    pub id: Value,
    pub op: Op,
}

/// The operations an external agent may ask for.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    /// The transcript lines of `agent` from `since` (0-based, inclusive), with
    /// a revision the client hands back to [`Op::Edit`].
    Read { agent: u64, since: usize },
    /// The roster the tree pane paints, so a client can see the whole tree.
    Agents,
    /// Focus `agent` exactly as `Enter` on its row does.
    Focus { agent: u64 },
    /// Set the message box's draft for `agent` (or, with `send`, deliver
    /// `text` as the human's message), refused unless the transcript still
    /// stands at `base`.
    Edit {
        agent: u64,
        base: u64,
        text: String,
        send: bool,
    },
}

/// A line that is not a request, with whatever `id` could still be read.
#[derive(Clone, Debug, PartialEq)]
pub struct BadRequest {
    pub id: Value,
    pub message: String,
}

/// Parse one request line. Bad JSON, a missing or unknown `op`, or a field of
/// the wrong type is an error; one bad line never ends the connection.
pub fn parse_request(line: &str) -> Result<Request, BadRequest> {
    let value: Value = serde_json::from_str(line).map_err(|error| BadRequest {
        id: Value::Null,
        message: format!("not JSON: {error}"),
    })?;
    let Value::Object(object) = value else {
        return Err(BadRequest {
            id: Value::Null,
            message: "request is not a JSON object".to_string(),
        });
    };
    let id = object.get("id").cloned().unwrap_or(Value::Null);
    let bad = |message: &str| BadRequest {
        id: id.clone(),
        message: message.to_string(),
    };
    let name = object
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("`op` must be a string"))?;
    let agent = |object: &serde_json::Map<String, Value>| {
        object
            .get("agent")
            .and_then(Value::as_u64)
            .ok_or_else(|| bad("`agent` must be a non-negative integer"))
    };
    let op = match name {
        "read" => Op::Read {
            agent: agent(&object)?,
            since: match object.get("since") {
                None | Some(Value::Null) => 0,
                Some(value) => value
                    .as_u64()
                    .ok_or_else(|| bad("`since` must be a non-negative integer"))?
                    as usize,
            },
        },
        "agents" => Op::Agents,
        "focus" => Op::Focus {
            agent: agent(&object)?,
        },
        "edit" => Op::Edit {
            agent: agent(&object)?,
            base: object
                .get("base")
                .and_then(Value::as_u64)
                .ok_or_else(|| bad("`base` must be a non-negative integer"))?,
            text: object
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("`text` must be a string"))?
                .to_string(),
            send: match object.get("send") {
                None | Some(Value::Null) => false,
                Some(value) => value
                    .as_bool()
                    .ok_or_else(|| bad("`send` must be true or false"))?,
            },
        },
        other => return Err(bad(&format!("unknown op `{other}`"))),
    };
    Ok(Request { id, op })
}

impl Request {
    /// The line this request is sent as. One place the wire shape is written,
    /// so the CLI cannot spell a field the parser reads differently.
    pub fn encode(&self) -> String {
        let mut object = serde_json::Map::new();
        object.insert("id".to_string(), self.id.clone());
        match &self.op {
            Op::Read { agent, since } => {
                object.insert("op".to_string(), json!("read"));
                object.insert("agent".to_string(), json!(agent));
                object.insert("since".to_string(), json!(since));
            }
            Op::Agents => {
                object.insert("op".to_string(), json!("agents"));
            }
            Op::Focus { agent } => {
                object.insert("op".to_string(), json!("focus"));
                object.insert("agent".to_string(), json!(agent));
            }
            Op::Edit {
                agent,
                base,
                text,
                send,
            } => {
                object.insert("op".to_string(), json!("edit"));
                object.insert("agent".to_string(), json!(agent));
                object.insert("base".to_string(), json!(base));
                object.insert("text".to_string(), json!(text));
                object.insert("send".to_string(), json!(send));
            }
        }
        Value::Object(object).to_string()
    }
}

/// One answer, ready to serialize as a line.
#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    pub id: Value,
    pub reply: Reply,
}

impl Response {
    pub fn ok(id: Value, body: Value) -> Self {
        Self {
            id,
            reply: Reply::Ok(body),
        }
    }

    pub fn error(id: Value, error: ReplyError) -> Self {
        Self {
            id,
            reply: Reply::Err(error),
        }
    }

    /// The line this response is written as, without the trailing newline. JSON
    /// escapes every newline in the payload, so one response is always one line.
    pub fn encode(&self) -> String {
        let mut object = serde_json::Map::new();
        object.insert("id".to_string(), self.id.clone());
        match &self.reply {
            Reply::Ok(body) => {
                object.insert("ok".to_string(), body.clone());
            }
            Reply::Err(error) => {
                let mut body = serde_json::Map::new();
                body.insert("kind".to_string(), Value::String(error.kind.clone()));
                if let Some(message) = &error.message {
                    body.insert("message".to_string(), Value::String(message.clone()));
                }
                if let Some(revision) = error.revision {
                    body.insert("revision".to_string(), json!(revision));
                }
                object.insert("error".to_string(), Value::Object(body));
            }
        }
        Value::Object(object).to_string()
    }
}

/// What a response carries: a body, or an error.
#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    Ok(Value),
    Err(ReplyError),
}

/// A refusal. `conflict` carries the revision that moved instead of a message:
/// the client asked to edit a transcript that is no longer there, and the one
/// number it needs is how far it went.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplyError {
    pub kind: String,
    pub message: Option<String>,
    pub revision: Option<u64>,
}

impl ReplyError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            kind: "bad_request".to_string(),
            message: Some(message.into()),
            revision: None,
        }
    }

    pub fn conflict(revision: u64) -> Self {
        Self {
            kind: "conflict".to_string(),
            message: None,
            revision: Some(revision),
        }
    }

    /// The request was read and mush cannot answer it *now* — it is shutting
    /// down, or the UI dropped it. Answered as `bad_request` twice, which told
    /// a client that sent a perfectly good line to go and fix its syntax
    /// (finding A6); the two cases a client may usefully retry have their own
    /// kind.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: "unavailable".to_string(),
            message: Some(message.into()),
            revision: None,
        }
    }

    /// One line for a CLI to print on stderr.
    pub fn describe(&self) -> String {
        match (&self.message, self.revision) {
            (Some(message), _) => format!("{}: {message}", self.kind),
            (None, Some(revision)) => format!("{}: revision {revision} — read again", self.kind),
            (None, None) => self.kind.clone(),
        }
    }
}

/// Parse one response line (the CLI's side).
pub fn decode(line: &str) -> Result<Response, String> {
    let value: Value = serde_json::from_str(line).map_err(|error| format!("not JSON: {error}"))?;
    let id = value.get("id").cloned().unwrap_or(Value::Null);
    if let Some(body) = value.get("ok") {
        return Ok(Response::ok(id, body.clone()));
    }
    if let Some(error) = value.get("error") {
        return Ok(Response::error(
            id,
            ReplyError {
                kind: error
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("error")
                    .to_string(),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                revision: error.get("revision").and_then(Value::as_u64),
            },
        ));
    }
    Err("a response with neither `ok` nor `error`".to_string())
}

// ------------------------------------------------------------- response bodies

/// The shape of an `agents` answer, from the client's side: the keys
/// `App::attach_agents` writes, read back as a type rather than fished out of
/// a [`Value`] key by key.
///
/// The producer and the consumer each spelled the wire keys themselves, so a
/// key renamed on one side left the consumer's `field` closure returning `""`
/// and a client painted an empty column forever, with nothing failing (finding
/// R23). One shape per body says which fields must be there; [`Roster::read`]
/// is the only way to get one, and it *errors* on a key that went missing
/// rather than defaulting it. Extra keys the producer adds are ignored, so the
/// body can grow without a client breaking.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Roster {
    pub agents: Vec<RosterEntry>,
}

/// One `agents` row: what a client needs to paint the tree and to `edit`
/// against it. The optional fields are the ones the producer writes as `null`
/// (a root has no `parent`, an idle agent no `activity`).
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct RosterEntry {
    pub id: u64,
    pub parent: Option<u64>,
    pub phase: String,
    pub activity: Option<String>,
    pub title: String,
    pub branch: Option<String>,
    pub worktree: String,
    pub children_working: usize,
}

impl Roster {
    /// Read an `agents` body. A missing or wrongly-typed key names the key it
    /// could not read, so a wire drift is a client's error and not a blank
    /// column.
    pub fn read(body: &Value) -> Result<Roster, String> {
        serde_json::from_value(body.clone())
            .map_err(|error| format!("could not read the `agents` answer: {error}"))
    }
}

/// The shape of a `read` answer: the transcript lines, each with the index a
/// later `edit`'s `base` is not about but a `since` is. See [`Roster`] for why
/// this is a type and not a [`Value`].
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Transcript {
    pub lines: Vec<TranscriptLine>,
}

/// One transcript line: its 0-based index on the wire and its text, which may
/// hold newlines and is escaped by the printer (finding A3).
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct TranscriptLine {
    pub line: usize,
    pub text: String,
}

impl Transcript {
    /// Read a `read` body, reporting the key that would not read.
    pub fn read(body: &Value) -> Result<Transcript, String> {
        serde_json::from_value(body.clone())
            .map_err(|error| format!("could not read the `read` answer: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn round_trip(request: Request) -> Request {
        let line = request.encode();
        parse_request(&line).expect("its own line parses")
    }

    #[test]
    fn every_op_round_trips_through_its_line() {
        for request in [
            Request {
                id: json!(1),
                op: Op::Read {
                    agent: 2,
                    since: 12,
                },
            },
            Request {
                id: json!(7),
                op: Op::Agents,
            },
            Request {
                id: json!(3),
                op: Op::Focus { agent: 5 },
            },
            Request {
                id: json!("given"),
                op: Op::Edit {
                    agent: 0,
                    base: 34,
                    text: "a new line\nwith a break".to_string(),
                    send: true,
                },
            },
        ] {
            assert_eq!(round_trip(request.clone()), request, "{request:?}");
        }
    }

    #[test]
    fn missing_optional_fields_read_as_their_default() {
        // `since` absent is from the start; `send` absent is a draft.
        let read = parse_request(r#"{"id":1,"op":"read","agent":0}"#).unwrap();
        assert_eq!(read.op, Op::Read { agent: 0, since: 0 });
        let edit = parse_request(r#"{"id":2,"op":"edit","agent":0,"base":1,"text":"x"}"#).unwrap();
        assert_eq!(
            edit.op,
            Op::Edit {
                agent: 0,
                base: 1,
                text: "x".to_string(),
                send: false,
            }
        );
    }

    #[test]
    fn an_unknown_op_is_a_bad_request_with_its_id() {
        let error = parse_request(r#"{"id":9,"op":"dance"}"#).unwrap_err();
        assert_eq!(error.id, json!(9));
        assert!(error.message.contains("dance"), "{}", error.message);
    }

    #[test]
    fn malformed_json_is_a_bad_request_with_a_null_id() {
        let error = parse_request("{not json").unwrap_err();
        assert_eq!(error.id, Value::Null, "there was no id to read");
        assert!(!error.message.is_empty());

        let error = parse_request("[1,2,3]").unwrap_err();
        assert_eq!(error.id, Value::Null);
        assert!(error.message.contains("object"), "{}", error.message);
    }

    #[test]
    fn a_wrongly_typed_field_is_a_bad_request_that_keeps_the_id() {
        let error = parse_request(r#"{"id":4,"op":"read","agent":"zero"}"#).unwrap_err();
        assert_eq!(error.id, json!(4), "the id survived the bad field");
        assert!(error.message.contains("agent"), "{}", error.message);

        let error =
            parse_request(r#"{"id":5,"op":"edit","agent":0,"base":1,"text":7}"#).unwrap_err();
        assert_eq!(error.id, json!(5));
        assert!(error.message.contains("text"), "{}", error.message);
    }

    /// A `null` id is a legal one and must come back as `null`, not be replaced
    /// by a number the client never sent.
    #[test]
    fn a_null_id_survives_the_round_trip() {
        let request = parse_request(r#"{"id":null,"op":"agents"}"#).unwrap();
        assert_eq!(request.id, Value::Null);
        let response = Response::ok(request.id.clone(), json!({"agents": []}));
        assert_eq!(decode(&response.encode()).unwrap().id, Value::Null);

        // And a request that carried no id at all answers with `null` too.
        let error = parse_request(r#"{"op":"nope"}"#).unwrap_err();
        assert_eq!(error.id, Value::Null);
    }

    #[test]
    fn a_response_is_one_line_and_round_trips() {
        let ok = Response::ok(
            json!(1),
            json!({"agent": 0, "revision": 3, "lines": [{"line": 0, "role": "user", "text": "hi\nthere"}]}),
        );
        let line = ok.encode();
        assert!(!line.contains('\n'), "no raw newline: {line}");
        assert_eq!(decode(&line).unwrap(), ok);

        let conflict = Response::error(json!(4), ReplyError::conflict(37));
        assert_eq!(
            conflict.encode(),
            r#"{"error":{"kind":"conflict","revision":37},"id":4}"#
        );
        assert_eq!(decode(&conflict.encode()).unwrap(), conflict);

        let bad = Response::error(json!(null), ReplyError::bad_request("no op"));
        assert_eq!(
            decode(&bad.encode()).unwrap().reply,
            Reply::Err(ReplyError {
                kind: "bad_request".to_string(),
                message: Some("no op".to_string()),
                revision: None,
            })
        );
    }

    // ------------------------------------------------------------- over a socket

    /// The whole transport, on a real socket in a scratch directory: a request
    /// answered, a bad line answered *and the connection kept*, then a valid
    /// line again — which is the property the smoke suite's other sockets do
    /// not exercise. `serve` spawns the accept thread; this test stands in for
    /// the UI, reading `Msg::Attach` off the channel and replying.
    #[test]
    fn a_socket_answers_a_request_a_bad_line_and_then_more() {
        let root = std::env::temp_dir().join(format!("mush-attach-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(mush_core::session::MUSH_DIR)).unwrap();

        let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
        let guard = serve(&root, tx).unwrap();
        let socket = socket_path(&root);
        assert!(socket.exists(), "the socket is bound at {socket:?}");

        let mut client = UnixStream::connect(&socket).unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());

        // A request that reaches the UI, answered by the stand-in `App`.
        writeln!(client, r#"{{"id":1,"op":"agents"}}"#).unwrap();
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Msg::Attach { request, reply, .. } => {
                assert_eq!(request.op, Op::Agents);
                reply
                    .send(Response::ok(request.id, json!({"agents": []})))
                    .unwrap();
            }
            _ => panic!("expected Msg::Attach"),
        }
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(decode(line.trim_end()).unwrap().id, json!(1));

        // A bad line is answered from the socket thread, without the UI.
        writeln!(client, "not json at all").unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        let reply = decode(line.trim_end()).unwrap();
        assert_eq!(reply.id, Value::Null);
        assert!(matches!(reply.reply, Reply::Err(_)));
        assert!(
            rx.try_recv().is_err(),
            "a bad line must not wake the UI thread"
        );

        // And the same connection still works afterwards.
        writeln!(client, r#"{{"id":2,"op":"agents"}}"#).unwrap();
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Msg::Attach { request, reply, .. } => {
                reply
                    .send(Response::ok(request.id, json!({"agents": []})))
                    .unwrap();
            }
            _ => panic!("expected Msg::Attach"),
        }
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert_eq!(decode(line.trim_end()).unwrap().id, json!(2));

        drop(client);
        drop(guard);
        assert!(!socket.exists(), "the guard removed the socket file");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// One idle client must not hold the surface: a connection that says
    /// nothing gets its own thread, so another client is answered while the
    /// first is still open. Accepting serially, a held socket made `mush
    /// agents` hang for as long as the holder felt like it (finding A2).
    #[test]
    fn an_idle_client_does_not_wedge_the_socket() {
        let root = std::env::temp_dir().join(format!("mush-attach-idle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(mush_core::session::MUSH_DIR)).unwrap();

        let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
        let guard = serve(&root, tx).unwrap();
        let socket = socket_path(&root);

        // A client that connects and says nothing at all.
        let held = UnixStream::connect(&socket).unwrap();

        // Another client is answered anyway, while the first is still open.
        let mut other = UnixStream::connect(&socket).unwrap();
        writeln!(other, r#"{{"id":7,"op":"agents"}}"#).unwrap();
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Msg::Attach { request, reply, .. } => {
                assert_eq!(request.id, json!(7));
                reply
                    .send(Response::ok(request.id, json!({ "agents": [] })))
                    .unwrap();
            }
            _ => panic!("expected Msg::Attach"),
        }
        let mut reader = BufReader::new(other);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(decode(line.trim_end()).unwrap().id, json!(7));

        drop(held);
        drop(guard);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A request mush cannot answer *now* is `unavailable`, not a
    /// `bad_request`: a client that sent a perfectly good line must not be told
    /// to go and fix its syntax (finding A6).
    #[test]
    fn unavailable_is_its_own_kind() {
        let error = ReplyError::unavailable("mush is shutting down");
        let response = Response::error(json!(3), error);
        let decoded = decode(&response.encode()).unwrap();
        match decoded.reply {
            Reply::Err(error) => {
                assert_eq!(error.kind, "unavailable");
                assert_eq!(error.describe(), "unavailable: mush is shutting down");
            }
            Reply::Ok(_) => panic!("expected an error"),
        }
    }

    /// A socket file with nothing listening behind it is a crash's leftover and
    /// is cleared; a file a live listener holds is another mush, and it is not
    /// stolen — the bind fails and the caller runs without attach.
    #[test]
    fn a_stale_socket_is_cleared_and_a_live_one_is_not_stolen() {
        let (tx, _rx) = crossbeam_channel::unbounded::<Msg>();

        // A file a live listener holds is another mush: the bind refuses it
        // rather than stealing its socket, and leaves the file where it was.
        let live_root =
            std::env::temp_dir().join(format!("mush-attach-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&live_root);
        std::fs::create_dir_all(live_root.join(mush_core::session::MUSH_DIR)).unwrap();
        let live_path = socket_path(&live_root);
        let live = UnixListener::bind(&live_path).unwrap();
        assert!(
            serve(&live_root, tx.clone()).is_err(),
            "a live socket is refused, not stolen"
        );
        assert!(live_path.exists(), "and it is left where it was");
        drop(live);
        let _ = std::fs::remove_dir_all(&live_root);

        // A stale file — nothing listening behind it, the shape a crash leaves
        // — is cleared and replaced. A root of its own, so the live phase's
        // probe (which leaves an unaccepted connection in that listener's
        // backlog) cannot outlive the listener it belongs to (finding A9).
        let root = std::env::temp_dir().join(format!("mush-attach-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(mush_core::session::MUSH_DIR)).unwrap();
        let path = socket_path(&root);
        let stale = UnixListener::bind(&path).unwrap();
        drop(stale);

        // Even so, a just-closed listener can answer for a moment while the
        // kernel tears the socket down, and `serve`'s liveness probe would
        // read the stale file as live and refuse to clear it. Wait, bounded,
        // for it to actually refuse before asking `serve` to clear it
        // (finding A9).
        let mut waits = 0;
        while UnixStream::connect(&path).is_ok() {
            waits += 1;
            assert!(waits < 200, "a closed listener kept answering {path:?}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let guard = serve(&root, tx).unwrap();
        assert!(UnixStream::connect(&path).is_ok(), "the new socket is live");
        drop(guard);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The three bounds on the surface: a request line past the cap is refused
    /// with the cap named and the connection kept; connections past the cap are
    /// refused with `unavailable`; and a client that says nothing is reaped by
    /// the idle window. The window is the socket's own read timeout, injected
    /// short, so the test waits on the client's read rather than on a sleep of
    /// its own.
    #[test]
    fn a_request_line_is_capped_and_a_client_is_reaped() {
        // (1) The line cap, under the production limits.
        let root = std::env::temp_dir().join(format!("mush-attach-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(mush_core::session::MUSH_DIR)).unwrap();
        let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
        let guard = serve_with(&root, tx, LIMITS, spawn_accept_loop).unwrap();
        let socket = socket_path(&root);
        let mut client = UnixStream::connect(&socket).unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        client
            .write_all(&vec![b'x'; MAX_REQUEST_BYTES + 1])
            .and_then(|()| client.write_all(b"\n"))
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        match decode(line.trim_end()).unwrap().reply {
            Reply::Err(error) => {
                assert_eq!(
                    error.kind, "bad_request",
                    "a line past the cap is a bad request"
                );
                let message = error.message.expect("the refusal names the cap");
                assert!(
                    message.contains(&MAX_REQUEST_BYTES.to_string()),
                    "the cap is named: {message}"
                );
            }
            Reply::Ok(_) => panic!("a line past the cap must be refused"),
        }
        assert!(
            rx.try_recv().is_err(),
            "a refused line never reaches the UI thread"
        );

        // The same connection answers a good line afterwards: the cap refuses
        // the line, not the client.
        client.write_all(b"{\"id\":1,\"op\":\"agents\"}\n").unwrap();
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Msg::Attach { request, reply, .. } => {
                assert_eq!(request.id, json!(1));
                reply
                    .send(Response::ok(request.id, json!({"agents": []})))
                    .unwrap();
            }
            _ => panic!("expected Msg::Attach"),
        }
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert!(
            matches!(decode(line.trim_end()).unwrap().reply, Reply::Ok(_)),
            "the connection survived the refused line"
        );
        drop(reader);
        drop(client);
        drop(guard);
        let _ = std::fs::remove_dir_all(&root);

        // (2) The connection cap: with room for two, the third client is
        // answered `unavailable` without a thread of its own. The two idle
        // clients hold their slots because this server's idle window is the
        // production one.
        let crowd = std::env::temp_dir().join(format!("mush-attach-crowd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&crowd);
        std::fs::create_dir_all(crowd.join(mush_core::session::MUSH_DIR)).unwrap();
        let (tx, _rx) = crossbeam_channel::unbounded::<Msg>();
        let guard = serve_with(
            &crowd,
            tx,
            Limits {
                connections: 2,
                idle: IDLE_TIMEOUT,
            },
            spawn_accept_loop,
        )
        .unwrap();
        let crowd_socket = socket_path(&crowd);
        let _first = UnixStream::connect(&crowd_socket).unwrap();
        let _second = UnixStream::connect(&crowd_socket).unwrap();
        let third = UnixStream::connect(&crowd_socket).unwrap();
        let mut reader = BufReader::new(third.try_clone().unwrap());
        line.clear();
        reader.read_line(&mut line).unwrap();
        match decode(line.trim_end()).unwrap().reply {
            Reply::Err(error) => {
                assert_eq!(error.kind, "unavailable", "a full surface says retry");
                assert!(
                    error.message.unwrap_or_default().contains('2'),
                    "and it names the cap"
                );
            }
            Reply::Ok(_) => panic!("a connection past the cap must be refused"),
        }
        drop(reader);
        drop(third);
        drop(guard);
        let _ = std::fs::remove_dir_all(&crowd);

        // (3) The idle window: a client that connects and says nothing is
        // reaped. The wait is the socket's own timeout — short here — and the
        // client reads the close instead of sleeping on it.
        let idle = std::env::temp_dir().join(format!("mush-attach-idle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&idle);
        std::fs::create_dir_all(idle.join(mush_core::session::MUSH_DIR)).unwrap();
        let (tx, _rx) = crossbeam_channel::unbounded::<Msg>();
        let guard = serve_with(
            &idle,
            tx,
            Limits {
                connections: MAX_CONNECTIONS,
                idle: Duration::from_millis(250),
            },
            spawn_accept_loop,
        )
        .unwrap();
        let silent = UnixStream::connect(socket_path(&idle)).unwrap();
        silent
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let started = std::time::Instant::now();
        line.clear();
        let read = BufReader::new(silent).read_line(&mut line).unwrap();
        let waited = started.elapsed();
        assert_eq!(read, 0, "the silent client was answered with a close");
        assert!(
            waited >= Duration::from_millis(25) && waited < Duration::from_secs(5),
            "the close came from the idle window, not at once: {waited:?}"
        );
        drop(guard);
        let _ = std::fs::remove_dir_all(&idle);
    }

    /// The cap is on the line, not near it: exactly `cap` bytes are a line and
    /// `cap + 1` are not, and it is the refused line's newline that leaves the
    /// next line whole.
    #[test]
    fn a_line_at_the_cap_is_kept_and_a_line_past_it_is_refused() {
        let at_cap = format!(
            "{}\n{{\"id\":1,\"op\":\"agents\"}}\n",
            "x".repeat(MAX_REQUEST_BYTES)
        );
        let mut reader = BufReader::new(std::io::Cursor::new(at_cap.into_bytes()));
        match read_line_capped(&mut reader, MAX_REQUEST_BYTES).unwrap() {
            Line::Text(line) => assert_eq!(line.len(), MAX_REQUEST_BYTES),
            other => panic!("a line exactly at the cap is a line: {other:?}"),
        }
        match read_line_capped(&mut reader, MAX_REQUEST_BYTES).unwrap() {
            Line::Text(line) => assert_eq!(line, r#"{"id":1,"op":"agents"}"#),
            other => panic!("the line after the cap is read whole: {other:?}"),
        }

        let past = format!("{}\nstill a request\n", "x".repeat(MAX_REQUEST_BYTES + 1));
        let mut reader = BufReader::new(std::io::Cursor::new(past.into_bytes()));
        assert!(matches!(
            read_line_capped(&mut reader, MAX_REQUEST_BYTES).unwrap(),
            Line::Oversize
        ));
        match read_line_capped(&mut reader, MAX_REQUEST_BYTES).unwrap() {
            Line::Text(line) => assert_eq!(line, "still a request"),
            other => panic!("the refused line ended at its newline: {other:?}"),
        }
    }

    /// A thread that will not start must not leave the socket file behind. The
    /// guard is built before the start, so the failed serve returns through its
    /// `Drop` and the file goes with it (finding A7).
    #[test]
    fn a_thread_that_will_not_start_takes_the_socket_with_it() {
        let root = std::env::temp_dir().join(format!("mush-attach-spawn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(mush_core::session::MUSH_DIR)).unwrap();
        let (tx, _rx) = crossbeam_channel::unbounded::<Msg>();

        let error = match serve_with(&root, tx, LIMITS, |_, _, _| {
            Err("no threads today".to_string())
        }) {
            Ok(_) => panic!("a start that fails fails the serve"),
            Err(error) => error,
        };
        assert_eq!(
            error, "no threads today",
            "the start's own words reach the caller"
        );
        assert!(
            !socket_path(&root).exists(),
            "the bound socket went with the failed serve"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
