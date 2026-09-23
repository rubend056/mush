//! A tiny blocking HTTP/1.1 client.
//!
//! mush only ever talks to OpenAI-compatible endpoints, so a whole HTTP stack
//! would be overkill. This handles exactly what we need: `Content-Length` or
//! chunked responses, over plain HTTP or TLS (rustls), and one connection kept
//! per endpoint and reused (`Connection: keep-alive`) instead of a fresh
//! TCP+TLS handshake for every call. Keeping it in-tree means no framework and
//! no runtime to debug.
//!
//! A chat request can be *watched*: the socket is read in short slices and the
//! caller's cancellation flag is polled between them, so Ctrl-C interrupts a
//! model that is still thinking instead of waiting for its reply.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use mush_core::Config;

use crate::clock::{self, Clock};

/// Fail fast when the endpoint is unreachable, rather than inheriting the
/// operating system's multi-minute connect timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A chat completion may legitimately take minutes on a slow local model. The
/// deadline bounds one whole request; it is no longer a per-read timeout.
const CHAT_READ_TIMEOUT: Duration = Duration::from_secs(600);
/// Listing models must never freeze the caller: the UI thread does this when
/// `/model`, `/url`, or `/key` runs, and a stalled endpoint should just fall
/// back to the provider's known list.
const LIST_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one socket read waits before the reader checks for a cancellation.
/// Short enough that a Stop lands promptly, long enough that a silent endpoint
/// costs a handful of wake-ups per second, not a spin.
const READ_SLICE: Duration = Duration::from_millis(200);
/// A request body is small; a write that blocks this long is a dead endpoint.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// A response body larger than this is refused while it is being read, so a
/// server cannot make mush allocate without bound (docs §8). Generous on
/// purpose: a big diff or a long model reply is normal work.
const MAX_BODY_BYTES: usize = 80 * 1024 * 1024;

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

/// Anything the client can read from and write to: a plain TCP stream or a
/// rustls TLS stream. `Box<dyn Read + Write>` is not a valid trait object, so
/// one supertrait is needed. `Send` because a kept connection is parked in the
/// pool a whole tree of actors shares.
trait ReadWrite: Read + Write + Send {}
impl<T: Read + Write + Send> ReadWrite for T {}

pub fn get_json(url: &str, api_key: Option<&str>, read_timeout: Duration) -> io::Result<Response> {
    request(
        &Ask {
            method: "GET",
            url,
            body: None,
            api_key,
            read_timeout,
            cancel: None,
        },
        clock::system(),
        &POOL,
        &mut connect,
    )
}

/// POST a chat completion. `cancel` is polled while the socket waits, so a Stop
/// reaches a model that has not answered yet — the difference between Ctrl-C
/// working in a moment and Ctrl-C working after the reply.
pub fn post_json(
    url: &str,
    body: &str,
    api_key: Option<&str>,
    cancel: &AtomicBool,
) -> io::Result<Response> {
    request(
        &Ask {
            method: "POST",
            url,
            body: Some(body),
            api_key,
            read_timeout: CHAT_READ_TIMEOUT,
            cancel: Some(cancel),
        },
        clock::system(),
        &POOL,
        &mut connect,
    )
}

/// One model the endpoint advertises. `context` is the window it reported, when
/// it reports one at all — llama.cpp's `meta.n_ctx`, vLLM's `max_model_len`,
/// OpenRouter's `context_length`. Hosted APIs answer with ids only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Model {
    pub id: String,
    pub context: Option<usize>,
}

/// List the models an endpoint advertises. Falls back to the provider's known
/// models when the endpoint is unreachable or lacks `/v1/models`.
pub fn list_models(cfg: &Config) -> Vec<Model> {
    let known = || {
        cfg.default_models()
            .into_iter()
            .map(|id| Model { id, context: None })
            .collect::<Vec<_>>()
    };
    let response = match get_json(&cfg.models_url(), cfg.api_key.as_deref(), LIST_READ_TIMEOUT) {
        Ok(response) if response.status == 200 => response,
        _ => return known(),
    };
    let value: serde_json::Value = match serde_json::from_str(&response.body) {
        Ok(value) => value,
        Err(_) => return known(),
    };
    let models: Vec<Model> = value
        .get("data")
        .and_then(serde_json::Value::as_array)
        .map(|list| list.iter().filter_map(model_of).collect())
        .unwrap_or_default();
    if models.is_empty() {
        known()
    } else {
        models
    }
}

fn model_of(value: &serde_json::Value) -> Option<Model> {
    let id = value.get("id").and_then(serde_json::Value::as_str)?;
    // Every server that advertises a window uses its own spelling for it.
    let context = ["max_model_len", "context_length", "context_window", "n_ctx"]
        .iter()
        .find_map(|key| {
            value
                .get(*key)
                .or_else(|| value.get("meta").and_then(|meta| meta.get(*key)))
                .and_then(serde_json::Value::as_u64)
        })
        .map(|tokens| tokens as usize)
        .filter(|tokens| *tokens > 0);
    Some(Model {
        id: id.to_string(),
        context,
    })
}

/// One request: what to send, and everything the transport needs beside it.
/// Gathered into a value because the transport takes more than a handful of
/// arguments now that the pool and the opener sit beside the request itself.
struct Ask<'a> {
    method: &'a str,
    url: &'a str,
    body: Option<&'a str>,
    api_key: Option<&'a str>,
    read_timeout: Duration,
    cancel: Option<&'a AtomicBool>,
}

/// The endpoint a connection belongs to. Reuse is per host, port and scheme,
/// because that is what a connection *is*: anything coarser would hand a stream
/// to a request that never spoke to it.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Endpoint {
    host: String,
    port: u16,
    tls: bool,
}

/// One connection to an endpoint, with the buffer reads go through. The pool
/// keeps it between requests, so it is both what a request reads and what a
/// kept connection is.
type Socket = BufReader<Box<dyn ReadWrite>>;

/// One connection per endpoint, kept for the next request to that endpoint.
///
/// Conservative on purpose: a connection is *taken* before it is used, so two
/// requests can never hold the same one, and it is only *kept* after a reply
/// that framed itself (a `Content-Length` or chunked body) and did not say
/// `Connection: close`. A body that was not read to its own end never goes
/// back: every failing road — the wire breaking, a cut-off body, a frame that
/// did not parse, a cancellation, a deadline — returns `Err` from `exchange`,
/// and `Err` leaves the pool empty. That is the rule that keeps a framing error
/// from being *produced* by our own reuse: the next request to this endpoint
/// opens a fresh connection, and no leftover chunk line can be read as its
/// reply (finding B27). Anything else — a body that ended at the stream's end,
/// a server that hangs up — leaves the pool empty too, so no request can ever
/// be handed a connection that failed. A kept connection the server has since
/// closed is retried once on a fresh one, and never counted twice.
#[derive(Default)]
struct Pool {
    /// Made on first keep rather than up front, so the pool can be a `static`
    /// that costs nothing until a connection is worth keeping.
    idle: Mutex<Option<HashMap<Endpoint, Socket>>>,
}

impl Pool {
    const fn new() -> Self {
        Self {
            idle: Mutex::new(None),
        }
    }

    /// Take the connection kept for this endpoint, if there is one. It is
    /// removed: the caller owns it for the request, and putting it back is what
    /// a successful, reusable reply does.
    fn take(&self, endpoint: &Endpoint) -> Option<Socket> {
        self.idle.lock().ok()?.as_mut()?.remove(endpoint)
    }

    /// Keep this connection for the next request to the same endpoint. One per
    /// endpoint: a newer stream replaces an older one rather than joining it,
    /// so a pool can never grow past the endpoints mush talks to.
    fn keep(&self, endpoint: Endpoint, stream: Socket) {
        if let Ok(mut idle) = self.idle.lock() {
            idle.get_or_insert_with(HashMap::new)
                .insert(endpoint, stream);
        }
    }

    #[cfg(test)]
    fn idle(&self) -> usize {
        self.idle
            .lock()
            .ok()
            .and_then(|idle| idle.as_ref().map(HashMap::len))
            .unwrap_or(0)
    }
}

/// The pool every real request goes through. One process is one mush, and one
/// endpoint is one host, so this is the tree's connection.
static POOL: Pool = Pool::new();

/// What opens a connection. The real client connects a socket; a test hands
/// back an in-memory stream, so reuse — and a kept connection that died — are
/// provable with no socket and no server.
type Open<'a> = &'a mut dyn FnMut(&str, u16, bool, Duration) -> io::Result<Box<dyn ReadWrite>>;

fn request(ask: &Ask<'_>, clock: &dyn Clock, pool: &Pool, open: Open<'_>) -> io::Result<Response> {
    let watch = Watch::new(ask.cancel, ask.read_timeout, clock);
    // A Stop that arrived before the request did: do not pay for a call the
    // human already cancelled.
    watch.check()?;

    let (host, port, path, tls) = parse_url(ask.url)?;
    let endpoint = Endpoint {
        host: host.clone(),
        port,
        tls,
    };

    // The connection the last request to this endpoint left behind, or a fresh
    // one. `reused` is what tells a dead kept connection apart from an endpoint
    // that will not talk to us.
    let pooled = pool.take(&endpoint);
    let reused = pooled.is_some();
    let stream = match pooled {
        Some(stream) => stream,
        None => BufReader::new(open(&host, port, tls, ask.read_timeout)?),
    };

    match exchange(stream, ask, &host, port, &path, &watch) {
        Ok((response, stream, reusable)) => {
            if reusable {
                pool.keep(endpoint, stream);
            }
            Ok(response)
        }
        Err((error, heard)) => {
            // A kept connection the server had already closed. Nothing was
            // heard from it — not one byte of an answer — so the request was
            // never answered and sending it again cannot duplicate anything.
            // One retry, only for a connection that was reused, and never for a
            // cancellation or a deadline: those are decisions, not a dead
            // socket, and must be reported as themselves. A cancellation is
            // the watch's own `Interrupted`, and `!watch.cancelled()` above
            // already excludes it: a signal that interrupted the socket was
            // retried inside the read or the write that met it. That leaves
            // `TimedOut` as the one kind to name here.
            let dead_kept =
                reused && !heard && !watch.cancelled() && error.kind() != io::ErrorKind::TimedOut;
            if !dead_kept {
                return Err(error);
            }
            watch.check()?;
            let fresh = BufReader::new(open(&host, port, tls, ask.read_timeout)?);
            match exchange(fresh, ask, &host, port, &path, &watch) {
                Ok((response, stream, reusable)) => {
                    if reusable {
                        pool.keep(endpoint, stream);
                    }
                    Ok(response)
                }
                Err((error, _)) => Err(error),
            }
        }
    }
}

/// One request and its reply on one connection.
///
/// `Ok` hands the connection back so the caller can keep it, with whether the
/// reply framed itself well enough to be worth keeping. `Err` says whether any
/// byte of the answer had arrived: nothing heard means the connection was dead
/// before the endpoint saw the request, which is the only failure a retry on a
/// fresh connection cannot duplicate.
///
/// A `Response` exists only for a reply whose body framed itself completely
/// (to the `Content-Length`, through the zero chunk and its trailer, or to the
/// stream's end). Every `Err` is therefore a reply of which *nothing was handed
/// over* — the wire broke, the body was cut off, a frame did not parse — and
/// that is the rule `model.rs::retrying` reads when it decides what may be
/// asked again, and why asking again cannot duplicate anything a run has read.
fn exchange(
    mut stream: Socket,
    ask: &Ask<'_>,
    host: &str,
    port: u16,
    path: &str,
    watch: &Watch,
) -> Result<(Response, Socket, bool), (io::Error, bool)> {
    let mut heard = false;
    if let Err(error) = write_request(&mut stream, ask, host, port, path, watch) {
        return Err((error, heard));
    }

    // A blank line is not an answer: a kept connection can carry one from the
    // exchange before it (a server's keep-alive probe, or framing that left a
    // CRLF behind — see `read_chunked`). Read as the status line it was
    // reported as `malformed status line: ""`, refusing a reply that had not
    // even started. Skipping it keeps `heard` false, so a connection that dies
    // after the blank line is still "nothing heard" and gets its one retry.
    let status_line = loop {
        match read_line(&mut stream, watch) {
            Ok(Some(line)) if line.is_empty() => continue,
            Ok(Some(line)) => break line,
            // Nothing at all came back: the peer closed the connection before it
            // answered (a kept connection the server has since dropped), which is
            // not the same thing as a malformed status line, and must not be
            // reported as one.
            Ok(None) => {
                return Err((
                    io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "the connection ended before it answered",
                    ),
                    heard,
                ))
            }
            Err(error) => return Err((error, heard)),
        }
    };
    heard = true;
    let status = match parse_status(&status_line) {
        Ok(status) => status,
        Err(error) => return Err((error, heard)),
    };

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    let mut close = false;
    loop {
        let line = match read_line(&mut stream, watch) {
            Ok(Some(line)) => line,
            // The headers ended at the stream's end: the framing they would
            // have given is simply absent, exactly as it was before.
            Ok(None) => break,
            Err(error) => return Err((error, heard)),
        };
        if line.is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            // A length we cannot parse is a broken message, not an absent one:
            // guessing "read to EOF" would silently change the framing.
            match value.trim().parse() {
                Ok(length) => content_length = Some(length),
                Err(_) => {
                    return Err((
                        framing(format!("malformed Content-Length: {:?}", value.trim())),
                        heard,
                    ))
                }
            }
        } else if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
            chunked = true;
        } else if lower.starts_with("connection:") && lower.contains("close") {
            close = true;
        }
    }

    let body = if chunked {
        read_chunked(&mut stream, watch)
    } else if let Some(len) = content_length {
        read_exact(&mut stream, len, watch)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    } else {
        read_to_end(&mut stream, watch).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    };
    let body = match body {
        Ok(body) => body,
        Err(error) => return Err((error, heard)),
    };

    // A connection is only worth keeping when the reply said where it ended: a
    // body framed as "until the stream closes" *is* the closed stream. HTTP/1.1
    // keeps the connection alive unless the reply says otherwise, so only an
    // HTTP/1.1 reply that did not say `close` is kept.
    let reusable =
        !close && (chunked || content_length.is_some()) && status_line.starts_with("HTTP/1.1");
    Ok((Response { status, body }, stream, reusable))
}

/// The request head and its body, written whole and flushed: the reply cannot
/// start before the endpoint has all of it, so a write error means the
/// connection was already dead.
///
/// The write goes through `get_mut`, because std's `BufReader` is read-only.
/// That is safe here and deliberate: the buffer holds bytes the *server* sent,
/// in order, and a new request does not make them any less the server's.
fn write_request(
    stream: &mut Socket,
    ask: &Ask<'_>,
    host: &str,
    port: u16,
    path: &str,
    watch: &Watch,
) -> io::Result<()> {
    let mut head = format!(
        "{} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: keep-alive\r\nAccept: application/json\r\n",
        ask.method
    );
    if let Some(key) = ask.api_key {
        head.push_str(&format!("Authorization: Bearer {key}\r\n"));
    }
    if let Some(body) = ask.body {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    head.push_str("\r\n");

    // The head, the body and the flush are the same request: a signal landing
    // on any of them is a call to make again, not a request to report — and
    // asking the watch first means a Stop still wins over the retry.
    let out = stream.get_mut();
    retrying_interrupted(Some(watch), || out.write_all(head.as_bytes()))?;
    if let Some(body) = ask.body {
        retrying_interrupted(Some(watch), || out.write_all(body.as_bytes()))?;
    }
    retrying_interrupted(Some(watch), || out.flush())
}

/// A cancellation flag and a deadline, threaded through one request's reads.
///
/// The socket is given [`READ_SLICE`] as its read timeout, so every read wakes
/// up quickly; `check` is what turns those wake-ups into a decision — keep
/// waiting, or stop because the human asked us to.
struct Watch<'a> {
    cancel: Option<&'a AtomicBool>,
    deadline: Instant,
    /// The clock the deadline is compared against. Real requests read the
    /// system one; a test reaches a deadline by advancing a fake, so the
    /// "the endpoint stopped responding" path does not cost the suite the
    /// timeout it is proving.
    clock: &'a dyn Clock,
}

impl<'a> Watch<'a> {
    fn new(cancel: Option<&'a AtomicBool>, timeout: Duration, clock: &'a dyn Clock) -> Self {
        Self {
            cancel,
            deadline: clock.now() + timeout,
            clock,
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel
            .map(|flag| flag.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    /// Consulted after every successful read *and* on every read timeout: a
    /// cancellation or a deadline has to be able to stop a body that keeps
    /// arriving in slices, not only a silent one.
    fn check(&self) -> io::Result<()> {
        if self.cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "request cancelled",
            ));
        }
        if self.clock.now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the endpoint stopped responding",
            ));
        }
        Ok(())
    }
}

/// Only the short read slice is expected to time out; anything else is real.
fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// Make one IO call, and make it again when a signal interrupted it.
///
/// `SIGWINCH` — the human resizing the terminal — is the everyday one: it can
/// land while mush is mid-read or mid-write, and the kernel then fails the
/// syscall with `EINTR` (`ErrorKind::Interrupted`) *before* it moved a byte.
/// Nothing about the endpoint changed: it never refused the request, and the
/// signal was addressed to mush, not to the socket. Retrying is the only honest
/// answer (finding B25); reporting the interrupt instead tells the human their
/// endpoint is broken when it was their window manager.
///
/// The one `Interrupted` that is not `EINTR` is the cancel flag's, as
/// [`Watch::check`] reports it, and that one must *not* be retried: the two are
/// told apart by asking the watch, never by looking at the error. `check` runs
/// on every interrupt, and its verdict — the human's Stop, or a deadline that
/// has passed — is returned as itself, so a cancelled request is still answered
/// in a moment instead of being retried around the signal.
///
/// `watch` is the request behind the call. It is `None` for a call that has no
/// request yet — the connect, which carries its own timeout and no cancel flag.
fn retrying_interrupted<T>(
    watch: Option<&Watch>,
    mut operation: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    loop {
        match operation() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                if let Some(watch) = watch {
                    watch.check()?;
                }
            }
            result => return result,
        }
    }
}

/// One bufferful from the socket, retrying the read slices. `Ok(None)` is EOF.
///
/// Everything below reads through `fill_buf`/`consume` rather than
/// `read_line`/`read_exact`: a timeout leaves the buffer untouched, so the
/// retry cannot lose a half-read line — which is exactly what a cancellation
/// arriving mid-body would otherwise do.
///
/// The watch is checked after every successful read too, not only on timeout:
/// a server dribbling one byte per slice would otherwise never let a
/// cancellation or the deadline through.
fn fill<'b, R: BufRead>(reader: &'b mut R, watch: &Watch) -> io::Result<Option<&'b [u8]>> {
    loop {
        // `fill_buf` hands back a borrow of the reader's own buffer, and a
        // borrow cannot travel into a `FnMut` — it is the reader that lives
        // across the retry, not the reader's bytes. The helper is therefore
        // asked only *how much* is there, a `usize`, and the buffer itself is
        // taken from the reader right after: on a full buffer that second
        // `fill_buf` is a slice of memory the read above already filled, not
        // another read, so nothing can happen in between it and the count.
        match retrying_interrupted(Some(watch), || reader.fill_buf().map(<[u8]>::len)) {
            Ok(0) => return Ok(None),
            Ok(_) => {
                watch.check()?;
                return Ok(Some(reader.fill_buf()?));
            }
            Err(error) if is_timeout(&error) => watch.check()?,
            Err(error) => return Err(error),
        }
    }
}

/// A line without its terminator, or `None` at end of stream.
fn read_line<R: BufRead>(reader: &mut R, watch: &Watch) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    loop {
        let Some(chunk) = fill(reader, watch)? else {
            if line.is_empty() {
                return Ok(None);
            }
            break;
        };
        let take = chunk
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(chunk.len());
        line.extend_from_slice(&chunk[..take]);
        reader.consume(take);
        if line.ends_with(b"\n") {
            break;
        }
    }
    while line
        .last()
        .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
    {
        line.pop();
    }
    Ok(Some(String::from_utf8_lossy(&line).into_owned()))
}

/// Exactly `len` bytes, or an error if the body ends early.
fn read_exact<R: BufRead>(reader: &mut R, len: usize, watch: &Watch) -> io::Result<Vec<u8>> {
    if len > MAX_BODY_BYTES {
        return Err(body_too_large());
    }
    let mut out = Vec::with_capacity(len.min(64 * 1024));
    while out.len() < len {
        let Some(chunk) = fill(reader, watch)? else {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the body ended before its Content-Length",
            ));
        };
        let take = (len - out.len()).min(chunk.len());
        out.extend_from_slice(&chunk[..take]);
        reader.consume(take);
    }
    Ok(out)
}

/// Everything up to end of stream, refusing to grow past the body cap.
fn read_to_end<R: BufRead>(reader: &mut R, watch: &Watch) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = fill(reader, watch)? {
        if out.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(body_too_large());
        }
        out.extend_from_slice(chunk);
        let take = chunk.len();
        reader.consume(take);
    }
    Ok(out)
}

fn body_too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("the response body is larger than {MAX_BODY_BYTES} bytes"),
    )
}

/// Connect to the endpoint. Every step here is bounded: the name lookup by
/// [`resolve_bounded`], the TCP connect by [`CONNECT_TIMEOUT`], the write by
/// [`WRITE_TIMEOUT`], and every read by the read watch (finding A19).
fn connect(
    host: &str,
    port: u16,
    tls: bool,
    read_timeout: Duration,
) -> io::Result<Box<dyn ReadWrite>> {
    let mut last_error = None;
    // Opening a connection happens before there is a request to cancel, so the
    // calls below have no watch: they retry the signal and are bounded by
    // their own timeouts.
    let name = host.to_string();
    let addresses = resolve_bounded(host, port, clock::system(), move || {
        retrying_interrupted(None, || (name.as_str(), port).to_socket_addrs())
            .map(|addresses| addresses.collect())
    })?;
    for address in addresses {
        let stream = match retrying_interrupted(None, || {
            TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
        }) {
            Ok(stream) => stream,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        // Liveness guards, not UX timers: a stalled endpoint must not pin a
        // thread (and, for the model list, the whole TUI) forever.
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
        if tls {
            // A TLS handshake is a conversation, not a read, so during setup
            // it gets the whole budget; only the reads after it get the short
            // slice that lets a cancellation land while the model thinks.
            stream.set_read_timeout(Some(read_timeout))?;
            let stream = tls_connect(host, stream)?;
            stream.sock.set_read_timeout(Some(READ_SLICE))?;
            return Ok(Box::new(stream));
        }
        stream.set_read_timeout(Some(READ_SLICE))?;
        return Ok(Box::new(stream));
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no address for {host}:{port}"),
        )
    }))
}

/// How long a name gets to resolve. std's `to_socket_addrs` cannot be given a
/// timeout, and it is the last step of `connect` that could hang past every
/// deadline mush sets (a dead DNS server, a wedged VPN) — so the lookup runs on
/// its own thread and this is the deadline the caller waits on (finding A19).
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);
/// The resolver wait loop's slice: short enough that a deadline or a shutdown
/// is noticed promptly, long enough not to spin.
const RESOLVE_SLICE: Duration = Duration::from_millis(50);

/// Resolve `host:port`, bounded by [`RESOLVE_TIMEOUT`] on `clock`.
///
/// The lookup itself still blocks on its own thread; what is bounded is the
/// *wait* for it. A lookup that outlives the deadline is abandoned, not killed
/// — the resolver thread belongs to the OS to collect — and the caller gets a
/// `TimedOut` naming the host, which is a fact it can report instead of
/// hanging. The clock is a parameter so a test reaches the deadline by
/// advancing a fake instead of waiting ten seconds (finding A19).
fn resolve_bounded(
    host: &str,
    port: u16,
    clock: &dyn Clock,
    lookup: impl FnOnce() -> io::Result<Vec<SocketAddr>> + Send + 'static,
) -> io::Result<Vec<SocketAddr>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("mush-resolve".to_string())
        .spawn(move || {
            // The receiver may be gone (the deadline won), and then the answer is
            // nobody's.
            let _ = tx.send(lookup());
        })
        .map_err(io::Error::other)?;
    let deadline = clock.now() + RESOLVE_TIMEOUT;
    loop {
        match rx.try_recv() {
            Ok(result) => return result,
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(io::Error::other(format!(
                    "the resolver for {host}:{port} went away"
                )))
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if clock.now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "{host}:{port} did not resolve within {}s",
                    RESOLVE_TIMEOUT.as_secs()
                ),
            ));
        }
        clock.sleep(RESOLVE_SLICE);
    }
}

/// Wrap a TCP connection in TLS for `https://` endpoints, verifying against
/// the standard web PKI roots. The handshake is driven here so request errors
/// surface before any body is written.
fn tls_connect(
    host: &str,
    tcp: TcpStream,
) -> io::Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("bad TLS host: {host}"))
    })?;
    let connection = rustls::ClientConnection::new(Arc::new(config), server_name)
        .map_err(|error| io::Error::other(format!("TLS setup failed: {error}")))?;
    let mut stream = rustls::StreamOwned::new(connection, tcp);
    // This drives the handshake, both directions of it. A signal landing in the
    // middle of one leaves rustls exactly where it was, so the way to finish it
    // is to ask again — and the way to lose a healthy endpoint is to report
    // `EINTR` as a bad TLS host instead.
    retrying_interrupted(None, || stream.flush())?; // completes the handshake
    Ok(stream)
}

/// A URL to connect to: scheme, host, port and path.
///
/// Deliberately strict — exactly the two schemes mush speaks, and a scheme is
/// required — and a redirect is *not* followed: a 3xx comes back as its status
/// and the caller reports the endpoint's own answer. Following one means
/// keeping the `Location` header (which this client discards), deciding whether
/// a 301/302/303 becomes a GET, and whether the API key may travel to another
/// host. Three decisions, not two lines: an endpoint that redirects is one to
/// point mush at directly.
fn parse_url(url: &str) -> io::Result<(String, u16, String, bool)> {
    let (rest, tls) = if let Some(rest) = url.strip_prefix("https://") {
        (rest, true)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (rest, false)
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("only http:// and https:// endpoints are supported (got {url})"),
        ));
    };
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], rest[index..].to_string()),
        None => (rest, "/".to_string()),
    };
    let default_port = if tls { 443 } else { 80 };
    // An IPv6 literal is bracketed, and the colons inside it are not a port
    // separator: `[::1]` and `[::1]:8080` are both valid authorities.
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, tail) = bracketed.split_once(']').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("bad IPv6 host in {url}"),
            )
        })?;
        let port = match tail.strip_prefix(':') {
            Some(port) => Some(parse_port(port, url)?),
            None if tail.is_empty() => None,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("bad authority in {url}"),
                ))
            }
        };
        (host.to_string(), port.unwrap_or(default_port))
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host.to_string(), parse_port(port, url)?),
            None => (authority.to_string(), default_port),
        }
    };
    if host.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("no host in {url}"),
        ));
    }
    Ok((host, port, path, tls))
}

fn parse_port(port: &str, url: &str) -> io::Result<u16> {
    port.parse::<u16>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("bad port in {url}")))
}

/// A reply whose framing broke before its body could be read: a status line or
/// `Content-Length` that is not one, a chunk size that is not hex, a chunk
/// terminator that is not the terminator the framing promised.
///
/// Its own type rather than a bare `InvalidData`, because the kind alone cannot
/// say whether the endpoint *answered* or its reply *broke on the way in* — and
/// the two want opposite treatment. A body past [`MAX_BODY_BYTES`] is an answer
/// mush refuses (a `Refused`, never retried); a frame that never parsed was
/// never handed to the caller, so asking again on a fresh connection cannot
/// duplicate anything the run has read, and the connection that carried the
/// broken frame is dropped rather than kept (finding B27, `model.rs`).
#[derive(Debug)]
struct Framing(String);

impl fmt::Display for Framing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Framing {}

/// A framing error, as [`Framing`] carries it.
fn framing(why: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, Framing(why.into()))
}

/// Whether `error` is a reply's framing breaking rather than an answer: the
/// distinction [`Framing`] exists for. `model.rs` asks this instead of matching
/// the message, so "what a framing error is" has one home.
pub fn is_framing(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.downcast_ref::<Framing>().is_some())
}

fn parse_status(line: &str) -> io::Result<u16> {
    line.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| framing(format!("malformed status line: {line:?}")))
}

/// A body that reached the end of the stream before its framing did.
///
/// `UnexpectedEof` — the wire failing under a reply the caller never saw — and
/// deliberately not `InvalidData`: read as a malformed chunk, a dropped
/// connection became "the endpoint sent a broken frame", was classified as the
/// endpoint refusing the request, and killed the run without a retry (finding
/// B27). The message is this body's own, because `read_exact`'s says
/// `Content-Length` and a chunked reply has none.
fn body_cut_off() -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "the reply ended inside its chunked body",
    )
}

/// Exactly `len` bytes of a chunked body, where the stream ending early is the
/// body being cut off rather than `read_exact`'s `Content-Length` complaint.
fn read_chunk_bytes<R: BufRead>(reader: &mut R, len: usize, watch: &Watch) -> io::Result<Vec<u8>> {
    read_exact(reader, len, watch).map_err(|error| match error.kind() {
        io::ErrorKind::UnexpectedEof => body_cut_off(),
        _ => error,
    })
}

/// The CRLF that ends a chunk's data.
///
/// A bare LF is accepted where a server writes one: how a peer terminates its
/// own chunks is not this reader's opinion to enforce, and insisting on the
/// `\r` used to eat the *first byte of the next size line*, turning a verbose
/// but perfectly readable body into `malformed chunk size: ""` — the exact
/// misdiagnosis finding B27 was filed for. Anything else is the framing
/// breaking, and says so with its own kind.
fn read_chunk_terminator<R: BufRead>(reader: &mut R, watch: &Watch) -> io::Result<()> {
    let first = read_chunk_bytes(reader, 1, watch)?[0];
    if first == b'\n' {
        return Ok(());
    }
    if first != b'\r' {
        return Err(framing(format!("malformed chunk terminator: {first:?}")));
    }
    let second = read_chunk_bytes(reader, 1, watch)?[0];
    if second != b'\n' {
        return Err(framing(format!(
            "malformed chunk terminator: {:?}",
            [first, second]
        )));
    }
    Ok(())
}

fn read_chunked<R: BufRead>(reader: &mut R, watch: &Watch) -> io::Result<String> {
    let mut out = Vec::new();
    loop {
        // A blank line where a chunk size belongs is framing noise, not a size:
        // some proxies write one, and a connection whose last body was
        // abandoned can leave one behind. `exchange` skips exactly this line
        // before a status line, and for the same reason. Whitespace counts as
        // blank too, so `"  \r\n"` cannot become `malformed chunk size: ""`.
        // A reply that then reaches the stream's end is a *cut-off* body, said
        // as one below.
        let size_line = loop {
            match read_line(reader, watch)? {
                Some(line) if line.trim().is_empty() => continue,
                Some(line) => break line,
                None => return Err(body_cut_off()),
            }
        };
        let size_field = size_line.trim().split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_field, 16)
            .map_err(|_| framing(format!("malformed chunk size: {size_field:?}")))?;
        if size == 0 {
            // The body ends at the zero chunk, but its framing does not: a
            // trailer section follows — `0 CRLF`, any trailer fields, then a
            // blank line. Left in the buffer, that blank line was read as the
            // *next* request's status line on a kept connection, which refused
            // a healthy reply as `malformed status line: ""`.
            loop {
                match read_line(reader, watch)? {
                    Some(line) if line.is_empty() => break,
                    // A trailer field: part of this body, not the next reply.
                    Some(_) => continue,
                    // The stream ended at the zero chunk; the body is complete.
                    None => break,
                }
            }
            break;
        }
        if out.len() + size > MAX_BODY_BYTES {
            return Err(body_too_large());
        }
        out.extend_from_slice(&read_chunk_bytes(reader, size, watch)?);
        read_chunk_terminator(reader, watch)?;
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::fake::Advanceable;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    #[test]
    fn parses_urls() {
        assert_eq!(
            parse_url("http://rubendpc:8078/v1/models").unwrap(),
            (
                "rubendpc".to_string(),
                8078,
                "/v1/models".to_string(),
                false
            )
        );
        assert_eq!(
            parse_url("http://localhost").unwrap(),
            ("localhost".to_string(), 80, "/".to_string(), false)
        );
        assert_eq!(
            parse_url("https://api.deepseek.com").unwrap(),
            ("api.deepseek.com".to_string(), 443, "/".to_string(), true)
        );
        assert_eq!(
            parse_url("https://api.deepseek.com:8443").unwrap(),
            ("api.deepseek.com".to_string(), 8443, "/".to_string(), true)
        );
        // An IPv6 literal's colons are not a port separator.
        assert_eq!(
            parse_url("http://[::1]/v1/models").unwrap(),
            ("::1".to_string(), 80, "/v1/models".to_string(), false)
        );
        assert_eq!(
            parse_url("https://[::1]:8443/x").unwrap(),
            ("::1".to_string(), 8443, "/x".to_string(), true)
        );
        assert!(parse_url("http://[::1").is_err());
        assert!(parse_url("http://[::1]:abc").is_err());
        assert!(parse_url("ftp://example.com").is_err());
    }

    #[test]
    fn parses_status_lines() {
        assert_eq!(parse_status("HTTP/1.1 200 OK").unwrap(), 200);
        assert_eq!(parse_status("HTTP/1.1 404 Not Found").unwrap(), 404);
        assert!(parse_status("garbage").is_err());
    }

    /// An endpoint that accepts one connection and then says nothing — the
    /// "model is still thinking" shape.
    fn silent_endpoint() -> u16 {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((connection, _)) = listener.accept() {
                std::thread::sleep(Duration::from_secs(30));
                drop(connection);
            }
        });
        port
    }

    /// An endpoint that accepts the connection and then says nothing must
    /// time out, not wedge the caller: this is what keeps a stalled
    /// `/v1/models` from freezing the TUI.
    #[test]
    fn a_silent_endpoint_times_out() {
        let url = format!("http://127.0.0.1:{}/v1/models", silent_endpoint());
        let started = Instant::now();
        let error = get_json(&url, None, Duration::from_millis(300)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "took {:?}",
            started.elapsed()
        );
    }

    /// A Stop has to reach a model that has not answered yet. The socket is
    /// read in 200 ms slices, so the flag is noticed within a slice instead of
    /// when the reply finally arrives.
    #[test]
    fn a_cancelled_chat_request_stops_at_once() {
        let cancel = Arc::new(AtomicBool::new(false));
        let setter = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            setter.store(true, Ordering::SeqCst);
        });

        let url = format!("http://127.0.0.1:{}/v1/chat/completions", silent_endpoint());
        let started = Instant::now();
        let error = post_json(&url, "{}", None, &cancel).unwrap_err();
        let elapsed = started.elapsed();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted, "{error}");
        assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
        assert!(
            elapsed >= Duration::from_millis(250),
            "returned before the cancel was asked for: {elapsed:?}"
        );
    }

    /// The `Watch` is what turns a stalled endpoint into a decision, and its
    /// clock is the one it was handed. Advancing a fake past the deadline is
    /// how a five-minute read timeout is proved in no time at all — the test
    /// above this one still covers the socket path with a real connection.
    #[test]
    fn a_watch_deadline_is_reached_by_advancing_the_clock() {
        let clock = Advanceable::new();
        let cancel = AtomicBool::new(false);
        let watch = Watch::new(Some(&cancel), Duration::from_secs(300), &clock);

        assert!(watch.check().is_ok(), "the deadline is in the future");
        clock.advance(Duration::from_secs(299));
        assert!(watch.check().is_ok(), "and it is a bound, not a guess");

        clock.advance(Duration::from_secs(1));
        let error = watch.check().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");

        // A Stop outranks the deadline: the human's answer beats the clock's.
        clock.advance(Duration::from_secs(300));
        cancel.store(true, Ordering::SeqCst);
        let error = watch.check().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted, "{error}");
    }

    /// A resolver that never answers must not hold the caller: the lookup runs
    /// on its own thread and the wait is bounded by the clock, so advancing a
    /// fake is the whole test. Before this, when the wait ended was the OS
    /// resolver's to decide (finding A19).
    #[test]
    fn a_resolver_that_never_answers_is_bounded_by_the_clock() {
        let clock = Advanceable::new();
        let started = Instant::now();
        let error = resolve_bounded("mush.invalid", 80, &clock, || {
            std::thread::sleep(Duration::from_secs(3600));
            Err(io::Error::other("too late"))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert!(error.to_string().contains("mush.invalid"), "{error}");
        assert!(
            clock.elapsed() >= RESOLVE_TIMEOUT,
            "the deadline is what ended it: {:?}",
            clock.elapsed()
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "and it was reached by moving the clock, not by waiting: {:?}",
            started.elapsed()
        );
    }

    /// The other road: a lookup that answers is returned as it is. The real
    /// clock, because this is the one case the fake cannot stage: the answer
    /// arrives on a thread, and a virtual wait that costs no time can reach the
    /// deadline before the OS ever schedules it.
    #[test]
    fn a_resolver_answer_is_returned() {
        let address: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let addresses =
            resolve_bounded("127.0.0.1", 1, clock::system(), move || Ok(vec![address])).unwrap();
        assert_eq!(addresses, vec![address]);
    }

    /// A cancellation that arrives before the request never pays for the call.
    #[test]
    fn an_already_cancelled_request_is_not_sent() {
        let cancel = AtomicBool::new(true);
        let started = Instant::now();
        // Port 1 is not listening: reaching the network at all would fail with
        // a connection error, not with `Interrupted`.
        let error = post_json(
            "http://127.0.0.1:1/v1/chat/completions",
            "{}",
            None,
            &cancel,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted, "{error}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// A connection a test drives by hand: `answers` are the raw bytes of the
    /// replies, served one per request, everything written is recorded, and an
    /// emptied queue is the stream's end. No socket, no server, no thread.
    struct Wire {
        answers: std::collections::VecDeque<Vec<u8>>,
        current: Vec<u8>,
        written: Arc<Mutex<Vec<u8>>>,
    }

    impl Read for Wire {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.current.is_empty() {
                match self.answers.pop_front() {
                    Some(answer) => self.current = answer,
                    // Nothing scripted left: the peer closed the connection,
                    // which is exactly how a server drops an idle one.
                    None => return Ok(0),
                }
            }
            let take = buf.len().min(self.current.len());
            buf[..take].copy_from_slice(&self.current[..take]);
            self.current.drain(..take);
            Ok(take)
        }
    }

    impl Write for Wire {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written
                .lock()
                .expect("no test panicked mid-write")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A connection that serves these replies, and records what is written.
    fn wire(written: &Arc<Mutex<Vec<u8>>>, answers: &[&str]) -> Wire {
        Wire {
            answers: answers.iter().map(|a| a.as_bytes().to_vec()).collect(),
            current: Vec::new(),
            written: written.clone(),
        }
    }

    /// An HTTP/1.1 200 with a `Content-Length` body, and any extra header lines
    /// the test wants — each written with its own CRLF, so `Connection: close\r\n`
    /// says what it looks like on the wire.
    fn ok_with(headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{headers}\r\n{body}",
            body.len()
        )
    }

    /// The shape every endpoint uses, and one a connection can be kept for.
    fn ok(body: &str) -> String {
        ok_with("", body)
    }

    /// One request through a pool and an opener the test owns — no socket, and
    /// the pool is this test's alone.
    fn send(pool: &Pool, open: Open<'_>, url: &str, body: &str) -> io::Result<Response> {
        let ask = Ask {
            method: "POST",
            url,
            body: Some(body),
            api_key: Some("secret"),
            read_timeout: Duration::from_secs(5),
            cancel: None,
        };
        request(&ask, clock::system(), pool, open)
    }

    /// Two calls to one endpoint must not cost two TCP+TLS handshakes: the kept
    /// connection serves the second request, and both replies parse.
    #[test]
    fn a_kept_connection_serves_the_next_request_to_the_same_endpoint() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let opened = Arc::new(AtomicUsize::new(0));
        let opens = opened.clone();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(wire(&written, &[&ok("{\"one\":1}"), &ok("{\"two\":2}")]));
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _timeout: Duration| {
            opens.fetch_add(1, Ordering::SeqCst);
            match queue.pop_front() {
                Some(wire) => Ok(Box::new(wire) as Box<dyn ReadWrite>),
                None => Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "the test scripted no more connections",
                )),
            }
        };
        let pool = Pool::new();
        let url = "http://models.test:8078/v1/chat/completions";

        let first = send(&pool, &mut opener, url, "{\"ask\":1}").unwrap();
        let second = send(&pool, &mut opener, url, "{\"ask\":2}").unwrap();
        assert_eq!(first.body, "{\"one\":1}");
        assert_eq!(second.body, "{\"two\":2}");
        assert_eq!(
            opened.load(Ordering::SeqCst),
            1,
            "one connection, two calls"
        );
        assert_eq!(pool.idle(), 1, "the connection is kept for the next call");

        // Both requests went down that one connection, and neither asked the
        // server to hang up after answering.
        let sent = String::from_utf8(written.lock().unwrap().clone()).unwrap();
        assert_eq!(sent.matches("POST /v1/chat/completions").count(), 2);
        assert_eq!(sent.matches("Connection: keep-alive").count(), 2);
        assert!(!sent.contains("Connection: close"), "{sent}");
        assert!(sent.contains("Authorization: Bearer secret"), "{sent}");
        assert!(sent.contains("{\"ask\":2}"), "the second body was sent");
    }

    /// A server closes an idle connection eventually. The stale one is
    /// discovered, dropped, and the request is answered on a fresh connection —
    /// not reported as a parse failure, and not retried forever.
    #[test]
    fn a_kept_connection_the_server_closed_is_replaced_once() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let opened = Arc::new(AtomicUsize::new(0));
        let opens = opened.clone();
        let mut queue = std::collections::VecDeque::new();
        // Serves one reply, then ends the stream (the server hung up).
        queue.push_back(wire(&written, &[&ok("{\"one\":1}")]));
        // The connection opened after that answers normally.
        queue.push_back(wire(&written, &[&ok("{\"two\":2}")]));
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _timeout: Duration| {
            opens.fetch_add(1, Ordering::SeqCst);
            match queue.pop_front() {
                Some(wire) => Ok(Box::new(wire) as Box<dyn ReadWrite>),
                None => Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "the test scripted no more connections",
                )),
            }
        };
        let pool = Pool::new();
        let url = "http://models.test:8078/v1/chat/completions";

        assert_eq!(
            send(&pool, &mut opener, url, "{}").unwrap().body,
            "{\"one\":1}"
        );
        let second = send(&pool, &mut opener, url, "{}").unwrap();
        assert_eq!(
            second.body, "{\"two\":2}",
            "the stale connection was replaced"
        );
        assert_eq!(opened.load(Ordering::SeqCst), 2, "one retry, no more");
        assert_eq!(pool.idle(), 1, "the fresh connection is kept");
    }

    /// A chunked reply ends with a trailer section, and the blank line that
    /// ends *that* belongs to the reply that is already over. Left in the
    /// buffer, it became the next request's "status line" and the endpoint's
    /// healthy answer was refused as `malformed status line: ""`.
    #[test]
    fn a_chunked_reply_does_not_poison_the_kept_connection() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let opened = Arc::new(AtomicUsize::new(0));
        let opens = opened.clone();
        let mut queue = std::collections::VecDeque::new();
        // One connection: a chunked reply (a trailer field, then the blank line
        // that ends the trailer), then the answer to the next request.
        let chunked = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                       Transfer-Encoding: chunked\r\n\r\n\
                       9\r\n{\"one\":1}\r\n0\r\nX-Trace: abc\r\n\r\n";
        queue.push_back(wire(&written, &[chunked, &ok("{\"two\":2}")]));
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _timeout: Duration| {
            opens.fetch_add(1, Ordering::SeqCst);
            match queue.pop_front() {
                Some(wire) => Ok(Box::new(wire) as Box<dyn ReadWrite>),
                None => Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "the test scripted no more connections",
                )),
            }
        };
        let pool = Pool::new();
        let url = "http://models.test:8078/v1/chat/completions";

        assert_eq!(
            send(&pool, &mut opener, url, "{}").unwrap().body,
            "{\"one\":1}"
        );
        let second = send(&pool, &mut opener, url, "{}").unwrap();
        assert_eq!(
            second.body, "{\"two\":2}",
            "the kept connection answers the next request"
        );
        assert_eq!(
            opened.load(Ordering::SeqCst),
            1,
            "the chunked reply's connection is reused cleanly"
        );
    }

    /// The live failure of finding B27, scripted byte for byte: a chunked reply
    /// whose connection ends between chunks. `read_line`'s end-of-stream used to
    /// become an *empty chunk-size line* — `malformed chunk size: ""` — and
    /// `InvalidData` is the class mush reads as "the endpoint deliberately sent
    /// this": refused, not retried, and the run ended blaming a healthy
    /// endpoint. A body that reached the stream's end inside its own framing is
    /// the wire failing under a reply nobody was handed, and says so.
    ///
    /// The second half is the pool rule: a body that was not read to its end is
    /// never kept, so the retry (or the next request) opens a fresh connection
    /// and cannot read leftover framing as its own reply.
    #[test]
    fn a_chunked_body_cut_off_before_its_zero_chunk_is_a_cut_off_body() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let opened = Arc::new(AtomicUsize::new(0));
        let opens = opened.clone();
        let mut queue = std::collections::VecDeque::new();
        // One whole chunk — size, data and terminator — and then the stream
        // ends: no next size line, no zero chunk, no trailer. This is what a
        // dropped connection looks like from inside `read_chunked`, and it is
        // the shape that used to be read as an empty chunk size.
        queue.push_back(wire(
            &written,
            &["HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
               Transfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n"],
        ));
        // The connection opened after it — the retry, or the next call —
        // answers normally.
        queue.push_back(wire(&written, &[&ok("{\"after\":1}")]));
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _timeout: Duration| {
            opens.fetch_add(1, Ordering::SeqCst);
            match queue.pop_front() {
                Some(wire) => Ok(Box::new(wire) as Box<dyn ReadWrite>),
                None => Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "the test scripted no more connections",
                )),
            }
        };
        let pool = Pool::new();
        let url = "http://models.test:8078/v1/chat/completions";

        let error = send(&pool, &mut opener, url, "{}").unwrap_err();
        assert_eq!(
            error.kind(),
            io::ErrorKind::UnexpectedEof,
            "a cut-off body is the wire failing, not a frame the endpoint sent: {error}"
        );
        assert_eq!(
            error.to_string(),
            "the reply ended inside its chunked body",
            "{error}"
        );
        assert!(
            !is_framing(&error),
            "a body cut off at the stream's end is the wire, and is classified like\
             a short Content-Length body: {error}"
        );
        assert_eq!(pool.idle(), 0, "a body that was not consumed is never kept");
        assert_eq!(
            opened.load(Ordering::SeqCst),
            1,
            "no retry hides inside one request"
        );

        // The connection that carried the broken body is gone: the next
        // request opens its own and reads its reply from the first byte.
        assert_eq!(
            send(&pool, &mut opener, url, "{}").unwrap().body,
            "{\"after\":1}"
        );
        assert_eq!(
            opened.load(Ordering::SeqCst),
            2,
            "the retry is on a fresh connection, never the broken one"
        );
    }

    /// A blank line where a chunk size belongs — some proxies write one, and an
    /// abandoned body can leave one behind — is skipped like the blank line
    /// `exchange` already skips before a status line. It is not a size, and it
    /// must not poison a reply that frames perfectly well behind it.
    #[test]
    fn a_stray_blank_line_before_a_chunk_size_is_skipped() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let opened = Arc::new(AtomicUsize::new(0));
        let opens = opened.clone();
        let mut queue = std::collections::VecDeque::new();
        // The blank line sits between the first chunk's terminator and the
        // zero chunk.
        let chunked = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                       Transfer-Encoding: chunked\r\n\r\n\
                       5\r\nhello\r\n\r\n0\r\n\r\n";
        queue.push_back(wire(&written, &[chunked, &ok("{\"two\":2}")]));
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _timeout: Duration| {
            opens.fetch_add(1, Ordering::SeqCst);
            match queue.pop_front() {
                Some(wire) => Ok(Box::new(wire) as Box<dyn ReadWrite>),
                None => Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "the test scripted no more connections",
                )),
            }
        };
        let pool = Pool::new();
        let url = "http://models.test:8078/v1/chat/completions";

        assert_eq!(send(&pool, &mut opener, url, "{}").unwrap().body, "hello");
        assert_eq!(
            send(&pool, &mut opener, url, "{}").unwrap().body,
            "{\"two\":2}"
        );
        assert_eq!(
            opened.load(Ordering::SeqCst),
            1,
            "the noise did not end a connection the reply framed"
        );
    }

    /// A chunk terminator written as a bare LF — a server variant seen in the
    /// wild — used to be read as `\r` + *the next size line's first byte*,
    /// which left `\r\n` where the size belongs and produced the same
    /// `malformed chunk size: ""` as a dropped connection. The terminator is
    /// the peer's to write; this reads the body it framed.
    #[test]
    fn a_chunk_terminated_with_a_bare_lf_is_read() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(wire(
            &written,
            &["HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\n0\r\n\r\n"],
        ));
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _timeout: Duration| match queue
            .pop_front()
        {
            Some(wire) => Ok(Box::new(wire) as Box<dyn ReadWrite>),
            None => Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "the test scripted no more connections",
            )),
        };
        let pool = Pool::new();

        assert_eq!(
            send(
                &pool,
                &mut opener,
                "http://models.test:8078/v1/chat/completions",
                "{}"
            )
            .unwrap()
            .body,
            "abc"
        );
    }

    /// The classification `model.rs` retries on: a frame that did not parse is a
    /// *framing* error — the reply broke on the way in, nothing of it was handed
    /// over — while a body past the cap is an answer mush refuses. The kind is
    /// `InvalidData` for both, which is exactly why the marker exists.
    #[test]
    fn a_frame_that_did_not_parse_is_marked_apart_from_a_refusal() {
        let huge = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        let cases: [(&str, bool); 4] = [
            // A chunk size that is not hex, a Content-Length that is not a
            // number, a chunk terminator the framing did not promise, and the
            // one answer mush refuses.
            (
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
                true,
            ),
            ("HTTP/1.1 200 OK\r\nContent-Length: twelve\r\n\r\n", true),
            (
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabcX",
                true,
            ),
            (&huge, false),
        ];
        for (answer, framed) in cases {
            let written = Arc::new(Mutex::new(Vec::new()));
            let mut queue = std::collections::VecDeque::new();
            queue.push_back(wire(&written, &[answer]));
            let mut opener =
                move |_host: &str, _port: u16, _tls: bool, _timeout: Duration| match queue
                    .pop_front()
                {
                    Some(wire) => Ok(Box::new(wire) as Box<dyn ReadWrite>),
                    None => Err(io::Error::new(
                        io::ErrorKind::ConnectionRefused,
                        "the test scripted no more connections",
                    )),
                };
            let pool = Pool::new();
            let error = send(
                &pool,
                &mut opener,
                "http://models.test:8078/v1/chat/completions",
                "{}",
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
            assert_eq!(is_framing(&error), framed, "{answer:?}: {error}");
        }
    }

    /// A Stop that lands while a chunked body is arriving abandons the reply
    /// where it stands, and the abandoned body is **not** what waits in the
    /// pool for the next request: the connection is dropped, and the next call
    /// opens a fresh one. This is the invariant that rules out the "our own
    /// leftover framing" hypothesis — nothing partially consumed ever goes
    /// back.
    #[test]
    fn a_cancelled_body_is_not_kept_for_the_next_request() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let opened = Arc::new(AtomicUsize::new(0));
        let opens = opened.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let head = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        let flag_at = head.len() + 3 + 2; // past the size line, inside `hello`
        let stopped = cancel.clone();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(Drip {
            inner: wire(&written, &[&format!("{head}5\r\nhello\r\n0\r\n\r\n")]),
            chunk: 4,
            served: 0,
            flag_at,
            cancel: stopped,
        });
        queue.push_back(Drip {
            inner: wire(&written, &[&ok("{\"after\":1}")]),
            chunk: 4,
            served: 0,
            flag_at: usize::MAX, // the Stop has already landed
            cancel: cancel.clone(),
        });
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _timeout: Duration| {
            opens.fetch_add(1, Ordering::SeqCst);
            match queue.pop_front() {
                Some(wire) => Ok(Box::new(wire) as Box<dyn ReadWrite>),
                None => Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "the test scripted no more connections",
                )),
            }
        };
        let pool = Pool::new();
        let ask = Ask {
            method: "POST",
            url: "http://models.test:8078/v1/chat/completions",
            body: Some("{}"),
            api_key: None,
            read_timeout: Duration::from_secs(5),
            cancel: Some(&cancel),
        };

        let error = request(&ask, clock::system(), &pool, &mut opener).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted, "{error}");
        assert_eq!(pool.idle(), 0, "the abandoned body is not kept");

        // The next request is served on a connection opened fresh, whose reply
        // is read from its first byte — no leftover chunk line can be read as
        // the status of a body that never started.
        cancel.store(false, Ordering::SeqCst);
        assert_eq!(
            send(&pool, &mut opener, ask.url, "{}").unwrap().body,
            "{\"after\":1}"
        );
        assert_eq!(opened.load(Ordering::SeqCst), 2, "a fresh connection");
    }

    /// A connection that serves its scripted reply a few bytes at a time, so a
    /// Stop can land *inside* a chunk instead of between replies. `flag_at` is
    /// the byte count after which the flag is set: one read delivers at most
    /// `chunk` bytes, so the cancel is observed at the next watch check.
    struct Drip {
        inner: Wire,
        chunk: usize,
        served: usize,
        flag_at: usize,
        cancel: Arc<AtomicBool>,
    }

    impl Read for Drip {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let take = buf.len().min(self.chunk);
            let mut scratch = vec![0u8; take];
            let read = self.inner.read(&mut scratch)?;
            buf[..read].copy_from_slice(&scratch[..read]);
            self.served += read;
            if self.served >= self.flag_at {
                self.cancel.store(true, Ordering::SeqCst);
            }
            Ok(read)
        }
    }

    impl Write for Drip {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.inner.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }

    /// A server can put a bare CRLF in front of a reply on an idle kept
    /// connection (a keep-alive probe, or a framing slip). It is not a status
    /// line: the reply behind it is read normally, on the same connection.
    #[test]
    fn a_blank_line_before_a_status_line_is_skipped() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let opened = Arc::new(AtomicUsize::new(0));
        let opens = opened.clone();
        let mut queue = std::collections::VecDeque::new();
        let second = format!("\r\n{}", ok("{\"two\":2}"));
        queue.push_back(wire(&written, &[&ok("{\"one\":1}"), &second]));
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _timeout: Duration| {
            opens.fetch_add(1, Ordering::SeqCst);
            match queue.pop_front() {
                Some(wire) => Ok(Box::new(wire) as Box<dyn ReadWrite>),
                None => Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "the test scripted no more connections",
                )),
            }
        };
        let pool = Pool::new();
        let url = "http://models.test:8078/v1/chat/completions";

        assert_eq!(
            send(&pool, &mut opener, url, "{}").unwrap().body,
            "{\"one\":1}"
        );
        assert_eq!(
            send(&pool, &mut opener, url, "{}").unwrap().body,
            "{\"two\":2}"
        );
        assert_eq!(
            opened.load(Ordering::SeqCst),
            1,
            "no reconnect: the blank line did not end the connection"
        );
    }

    /// The blank line and then the server is gone (the shape the pool sees
    /// when a server probes an idle connection and closes it). Nothing was
    /// heard from the connection, so the request is answered on a fresh one —
    /// not refused as a malformed status line.
    #[test]
    fn a_blank_line_then_a_closed_connection_is_retried() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let opened = Arc::new(AtomicUsize::new(0));
        let opens = opened.clone();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(wire(&written, &[&ok("{\"one\":1}"), "\r\n"]));
        queue.push_back(wire(&written, &[&ok("{\"two\":2}")]));
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _timeout: Duration| {
            opens.fetch_add(1, Ordering::SeqCst);
            match queue.pop_front() {
                Some(wire) => Ok(Box::new(wire) as Box<dyn ReadWrite>),
                None => Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "the test scripted no more connections",
                )),
            }
        };
        let pool = Pool::new();
        let url = "http://models.test:8078/v1/chat/completions";

        assert_eq!(
            send(&pool, &mut opener, url, "{}").unwrap().body,
            "{\"one\":1}"
        );
        let second = send(&pool, &mut opener, url, "{}").unwrap();
        assert_eq!(
            second.body, "{\"two\":2}",
            "the request was answered on a fresh connection"
        );
        assert_eq!(opened.load(Ordering::SeqCst), 2, "one retry, no more");
    }

    /// A connection whose reads a signal interrupts a few times before it
    /// answers — the shape of a `SIGWINCH` (the human resizing the terminal)
    /// landing on a read that is in flight. `stop` is a cancel flag the read
    /// sets as the signal lands, for the tests where the human's Ctrl-C has to
    /// be the winner.
    struct Interrupted {
        inner: Wire,
        interrupts: usize,
        reads: Arc<AtomicUsize>,
        stop: Option<Arc<AtomicBool>>,
    }

    impl Read for Interrupted {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if self.interrupts > 0 {
                self.interrupts -= 1;
                if let Some(stop) = &self.stop {
                    stop.store(true, Ordering::SeqCst);
                }
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Interrupted system call",
                ));
            }
            self.inner.read(buf)
        }
    }

    impl Write for Interrupted {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.inner.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }

    /// A signal landing on a read in flight is not the endpoint refusing the
    /// request: the read is made again and the reply arrives. Before the fix
    /// this failed with `Interrupted system call`, which the model layer
    /// reported as `cannot reach http://…` — a human resizing the window lost
    /// the turn, and the message blamed their endpoint (finding B25).
    #[test]
    fn a_signal_that_interrupts_a_read_is_retried() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let reads = Arc::new(AtomicUsize::new(0));
        let counted = reads.clone();
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _timeout: Duration| {
            Ok(Box::new(Interrupted {
                inner: wire(&written, &[&ok("{\"answer\":1}")]),
                interrupts: 3,
                reads: counted.clone(),
                stop: None,
            }) as Box<dyn ReadWrite>)
        };
        let pool = Pool::new();
        let url = "http://models.test:8078/v1/chat/completions";

        let response = send(&pool, &mut opener, url, "{}").unwrap();
        assert_eq!(response.body, "{\"answer\":1}");
        assert_eq!(
            reads.load(Ordering::SeqCst),
            4,
            "three interrupts made again, then the reply"
        );
    }

    /// The cancel flag still wins over the retry: a read a signal interrupted is
    /// *not* made again once the human has asked the request to stop. The watch
    /// reports a cancellation as an `Interrupted` of its own, so the helper has
    /// to ask the watch rather than treat every `Interrupted` as one more
    /// `EINTR` — treating them alike would swallow a Ctrl-C that arrived with a
    /// resize and leave the request waiting for an answer nobody wants.
    #[test]
    fn a_cancelled_request_is_not_retried_around_an_interrupt() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let reads = Arc::new(AtomicUsize::new(0));
        let counted = reads.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _timeout: Duration| {
            Ok(Box::new(Interrupted {
                inner: wire(&written, &[&ok("{\"never\":1}")]),
                interrupts: 1,
                reads: counted.clone(),
                stop: Some(stop.clone()),
            }) as Box<dyn ReadWrite>)
        };
        let ask = Ask {
            method: "POST",
            url: "http://models.test:8078/v1/chat/completions",
            body: Some("{}"),
            api_key: None,
            read_timeout: Duration::from_secs(5),
            cancel: Some(&cancel),
        };
        let pool = Pool::new();

        let error = request(&ask, clock::system(), &pool, &mut opener).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted, "{error}");
        assert_eq!(error.to_string(), "request cancelled", "the Stop's answer");
        assert_eq!(
            reads.load(Ordering::SeqCst),
            1,
            "the interrupted read was not made again"
        );
    }

    /// The retry helper on its own, with no socket: an interrupted call is made
    /// again, and every other answer — a real failure, a Stop, a deadline that
    /// has passed — is returned as itself without another attempt.
    #[test]
    fn retrying_interrupted_retries_only_an_interrupt() {
        let clock = Advanceable::new();
        let cancel = AtomicBool::new(false);
        let watch = Watch::new(Some(&cancel), Duration::from_secs(300), &clock);

        // Three signals in a row, then the answer: the caller sees the answer,
        // never the interrupts.
        let attempts = std::cell::Cell::new(0);
        let answer = retrying_interrupted(Some(&watch), || {
            attempts.set(attempts.get() + 1);
            if attempts.get() < 3 {
                Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Interrupted system call",
                ))
            } else {
                Ok(42)
            }
        })
        .unwrap();
        assert_eq!(answer, 42);
        assert_eq!(attempts.get(), 3);

        // A real failure is not a signal, and is not asked again.
        let attempts = std::cell::Cell::new(0);
        let error = retrying_interrupted(Some(&watch), || {
            attempts.set(attempts.get() + 1);
            Err::<(), _>(io::Error::new(io::ErrorKind::ConnectionReset, "reset"))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset, "{error}");
        assert_eq!(attempts.get(), 1);

        // A Stop outranks a signal: the watch is asked on every interrupt, and
        // its verdict is what the caller is handed.
        cancel.store(true, Ordering::SeqCst);
        let attempts = std::cell::Cell::new(0);
        let error = retrying_interrupted(Some(&watch), || {
            attempts.set(attempts.get() + 1);
            Err::<(), _>(io::Error::new(io::ErrorKind::Interrupted, "EINTR"))
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "request cancelled", "{error}");
        assert_eq!(attempts.get(), 1, "the call was not made again");

        // And so does the deadline, which needs no signal to have arrived.
        cancel.store(false, Ordering::SeqCst);
        clock.advance(Duration::from_secs(300));
        let attempts = std::cell::Cell::new(0);
        let error = retrying_interrupted(Some(&watch), || {
            attempts.set(attempts.get() + 1);
            Err::<(), _>(io::Error::new(io::ErrorKind::Interrupted, "EINTR"))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert_eq!(attempts.get(), 1);

        // A call with no request behind it — the connect — carries no watch and
        // still makes an interrupted call again.
        let attempts = std::cell::Cell::new(0);
        let connected = retrying_interrupted(None, || {
            attempts.set(attempts.get() + 1);
            if attempts.get() < 2 {
                Err(io::Error::new(io::ErrorKind::Interrupted, "EINTR"))
            } else {
                Ok("connected")
            }
        })
        .unwrap();
        assert_eq!(connected, "connected");
        assert_eq!(attempts.get(), 2);
    }

    /// A reply that ends the connection — `Connection: close`, or a body framed
    /// as "until the stream ends" — is not kept: the next call pays for its own
    /// connection rather than being handed a dead one.
    #[test]
    fn a_connection_the_reply_closed_is_not_kept() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let opened = Arc::new(AtomicUsize::new(0));
        let opens = opened.clone();
        let mut queue = std::collections::VecDeque::new();
        let closing = ok_with("Connection: close\r\n", "{\"x\":1}");
        queue.push_back(wire(&written, &[&closing]));
        queue.push_back(wire(&written, &[&ok("{\"y\":2}")]));
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _timeout: Duration| {
            opens.fetch_add(1, Ordering::SeqCst);
            match queue.pop_front() {
                Some(wire) => Ok(Box::new(wire) as Box<dyn ReadWrite>),
                None => Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "the test scripted no more connections",
                )),
            }
        };
        let pool = Pool::new();
        let url = "http://models.test:8078/v1/chat/completions";

        assert_eq!(
            send(&pool, &mut opener, url, "{}").unwrap().body,
            "{\"x\":1}"
        );
        assert_eq!(pool.idle(), 0, "a reply that closes is not kept");
        assert_eq!(
            send(&pool, &mut opener, url, "{}").unwrap().body,
            "{\"y\":2}"
        );
        assert_eq!(
            opened.load(Ordering::SeqCst),
            2,
            "the next call opened its own"
        );
    }

    /// A body framed as "until the stream ends" *is* the closed stream, so
    /// that connection cannot be kept either — the next call opens its own.
    #[test]
    fn a_reply_framed_to_the_end_of_the_stream_is_not_kept() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let opened = Arc::new(AtomicUsize::new(0));
        let opens = opened.clone();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(wire(
            &written,
            &["HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"eof\":1}"],
        ));
        queue.push_back(wire(&written, &[&ok("{\"y\":2}")]));
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _timeout: Duration| {
            opens.fetch_add(1, Ordering::SeqCst);
            match queue.pop_front() {
                Some(wire) => Ok(Box::new(wire) as Box<dyn ReadWrite>),
                None => Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "the test scripted no more connections",
                )),
            }
        };
        let pool = Pool::new();
        let url = "http://models.test:8078/v1/chat/completions";

        assert_eq!(
            send(&pool, &mut opener, url, "{}").unwrap().body,
            "{\"eof\":1}"
        );
        assert_eq!(
            pool.idle(),
            0,
            "a body read to the stream's end is not kept"
        );
        assert_eq!(
            send(&pool, &mut opener, url, "{}").unwrap().body,
            "{\"y\":2}"
        );
        assert_eq!(opened.load(Ordering::SeqCst), 2);
    }

    /// A connection that fails after the answer started is dropped, never put
    /// back, and the half-read answer is reported — a retry there could repeat
    /// work the endpoint has already done.
    #[test]
    fn a_connection_that_breaks_mid_answer_is_dropped_not_retried() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let opened = Arc::new(AtomicUsize::new(0));
        let opens = opened.clone();
        let mut queue = std::collections::VecDeque::new();
        // Announced as 20 bytes; only 3 arrive, then the stream ends.
        queue.push_back(wire(
            &written,
            &["HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n{\"a"],
        ));
        queue.push_back(wire(&written, &[&ok("{\"after\":1}")]));
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _timeout: Duration| {
            opens.fetch_add(1, Ordering::SeqCst);
            match queue.pop_front() {
                Some(wire) => Ok(Box::new(wire) as Box<dyn ReadWrite>),
                None => Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "the test scripted no more connections",
                )),
            }
        };
        let pool = Pool::new();
        let url = "http://models.test:8078/v1/chat/completions";

        // The failure is the one it is: a short body, not a parse error.
        let error = send(&pool, &mut opener, url, "{}").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof, "{error}");
        assert_eq!(
            opened.load(Ordering::SeqCst),
            1,
            "no retry of a heard request"
        );
        assert_eq!(pool.idle(), 0, "a failed connection is never kept");

        // And the pool is still usable: the next call opens its own.
        assert_eq!(
            send(&pool, &mut opener, url, "{}").unwrap().body,
            "{\"after\":1}"
        );
        assert_eq!(opened.load(Ordering::SeqCst), 2);
    }

    /// The 200 ms slice must not turn a slow-but-alive body into a failure:
    /// the endpoint dribbles its reply out over two slices' worth of silence.
    #[test]
    fn a_slow_body_is_not_a_timeout() {
        use std::io::Write as _;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut connection, _)) = listener.accept() {
                let mut scratch = [0u8; 1024];
                let _ = connection.read(&mut scratch);
                let _ = connection.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhe");
                let _ = connection.flush();
                std::thread::sleep(Duration::from_millis(600));
                let _ = connection.write_all(b"llo");
            }
        });

        let url = format!("http://127.0.0.1:{port}/v1/chat/completions");
        let response = get_json(&url, None, Duration::from_secs(5)).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "hello");
    }

    /// A server that keeps a byte per slice flowing must still be stopped by
    /// the deadline: the watch is consulted after every successful read, not
    /// only when a read times out (finding A1).
    #[test]
    fn a_dribbling_endpoint_still_hits_the_deadline() {
        let (url, handle) = dribbling_endpoint(60, 40);
        let started = Instant::now();
        let error = get_json(&url, None, Duration::from_millis(300)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "took {:?}",
            started.elapsed()
        );
        handle.join().unwrap();
    }

    /// The same for a cancellation: a Stop has to reach a body that is still
    /// arriving, not wait for it to end.
    #[test]
    fn a_cancelled_dribbling_response_stops() {
        let (url, handle) = dribbling_endpoint(200, 40);
        let cancel = Arc::new(AtomicBool::new(false));
        let setter = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            setter.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        let error = post_json(&url, "{}", None, &cancel).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted, "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "took {:?}",
            started.elapsed()
        );
        handle.join().unwrap();
    }

    /// A server that announces a long body and dribbles it a byte at a time
    /// with a short pause between slices. The connection is closed on drop.
    fn dribbling_endpoint(
        declared_len: usize,
        writes: usize,
    ) -> (String, std::thread::JoinHandle<()>) {
        use std::io::Write as _;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            if let Ok((mut connection, _)) = listener.accept() {
                let mut scratch = [0u8; 1024];
                let _ = connection.read(&mut scratch);
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {declared_len}\r\n\r\n");
                let _ = connection.write_all(head.as_bytes());
                for _ in 0..writes {
                    if connection.write_all(b".").is_err() {
                        return;
                    }
                    let _ = connection.flush();
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        });
        let url = format!("http://127.0.0.1:{port}/v1/chat/completions");
        (url, handle)
    }

    /// A `Content-Length` that cannot be parsed is a broken message, not an
    /// absent one: falling back to read-to-EOF would silently change framing.
    #[test]
    fn a_malformed_content_length_is_refused() {
        use std::io::Write as _;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut connection, _)) = listener.accept() {
                let mut scratch = [0u8; 1024];
                let _ = connection.read(&mut scratch);
                let _ = connection.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: abc\r\n\r\nbody");
                std::thread::sleep(Duration::from_secs(5));
            }
        });

        let url = format!("http://127.0.0.1:{port}/v1/models");
        let error = get_json(&url, None, Duration::from_secs(5)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
    }

    /// A body past the cap is refused while it is being read, however honest
    /// the framing is.
    #[test]
    fn an_oversized_body_is_refused() {
        use std::io::Write as _;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut connection, _)) = listener.accept() {
                let mut scratch = [0u8; 1024];
                let _ = connection.read(&mut scratch);
                // Announced length, and a chunked variant; neither body is sent.
                let _ = connection.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                        MAX_BODY_BYTES + 1
                    )
                    .as_bytes(),
                );
                let _ = connection.flush();
                std::thread::sleep(Duration::from_secs(5));
            }
        });

        let url = format!("http://127.0.0.1:{port}/v1/models");
        let started = Instant::now();
        let error = get_json(&url, None, Duration::from_secs(5)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// Talks to the configured endpoint; run with `--ignored`.
    #[test]
    #[ignore]
    fn live_models_endpoint() {
        let cfg = mush_core::Config::from_env();
        let response =
            get_json(&cfg.models_url(), cfg.api_key.as_deref(), CHAT_READ_TIMEOUT).unwrap();
        assert_eq!(response.status, 200);
        assert!(response.body.contains("data"));
    }

    /// The reply cap mush ships is one the configured endpoint actually takes.
    /// A cap past a vendor's documented `max_tokens` range is a 400 — worse
    /// than the short reply it would have stopped — so the number is checked
    /// against the endpoint itself rather than trusted. The request is built
    /// the way a run builds one, cap and all, and the endpoint's status is the
    /// verdict. Run with `--ignored`, pointing the usual `MUSH_*` at a live
    /// endpoint: a connection error is a fact about the network, not about the
    /// cap.
    #[test]
    #[ignore]
    fn live_endpoint_accepts_the_shipped_reply_cap() {
        // The same resolution startup runs (minus a home file), so the window —
        // and the cap that is a quarter of it — is the one a real run would
        // send, not `Config::from_env`'s un-derivable default.
        let mut cfg = mush_core::config::resolve(
            &mush_core::Overrides::from_env(),
            &mush_core::UserConfig::default(),
            None,
        )
        .expect("the environment resolves to a config")
        .config;
        if cfg.model.is_empty() {
            // Nothing named one: take what the endpoint lists first, so the
            // request names a model the endpoint knows.
            cfg.model = list_models(&cfg)
                .first()
                .map(|model| model.id.clone())
                .unwrap_or_default();
        }
        let request = mush_core::message::ChatRequest {
            model: &cfg.model,
            messages: &[mush_core::Message::user("say hi in one word")],
            tools: &[],
            tool_choice: "none",
            stream: false,
            temperature: cfg.temperature(),
            max_tokens: cfg.reply_cap(),
            max_completion_tokens: None,
            thinking: None,
            reasoning_effort: None,
        };
        let body = serde_json::to_string(&request).unwrap();
        let cancel = AtomicBool::new(false);
        let response = post_json(&cfg.chat_url(), &body, cfg.api_key.as_deref(), &cancel).unwrap();
        assert_eq!(
            response.status,
            200,
            "the endpoint refused the {}-token cap: {}",
            cfg.reply_cap(),
            response.body
        );
    }

    /// Proves the TLS path works against a public https endpoint. No key is
    /// used, so DeepSeek must answer 401 — a plain-HTTP-only client would fail
    /// to connect at all. Run with `--ignored`.
    #[test]
    #[ignore]
    fn live_https_tls_handshake() {
        let cfg = mush_core::Config {
            provider: mush_core::Provider::DeepSeek,
            base_url: "https://api.deepseek.com".to_string(),
            model: String::new(),
            api_key: None,
            context_tokens: 8192,
            context_explicit: false,
            temperature: mush_core::config::DEFAULT_TEMPERATURE,
            max_completion_tokens: false,
            reasoning_effort: None,
            thinking: None,
        };
        let response =
            get_json(&cfg.models_url(), cfg.api_key.as_deref(), CHAT_READ_TIMEOUT).unwrap();
        assert_eq!(response.status, 401);
    }
}
