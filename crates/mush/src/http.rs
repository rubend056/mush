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

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use mush_core::Config;

use crate::clock::{self, Clock};

/// The ceiling on one connect: fail fast when the endpoint is unreachable,
/// rather than inheriting the operating system's multi-minute connect timeout.
/// A *ceiling*, not a schedule — [`connect`] gives each address the smaller of
/// this and what is left of the call's deadline, so no connect can outlive the
/// call it serves.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Listing models must never freeze the caller: the UI thread does this when
/// `/model`, `/url`, or `/key` runs, and a stalled endpoint should just fall
/// back to the provider's known list. The number is that ask's *whole* budget:
/// every phase of it is bounded by the smaller of its own ceiling and what is
/// left, so a stalled lookup, connect or write cannot extend it either.
const LIST_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one socket read waits before the reader checks for a cancellation.
/// Short enough that a Stop lands promptly, long enough that a silent endpoint
/// costs a handful of wake-ups per second, not a spin.
const READ_SLICE: Duration = Duration::from_millis(200);
/// The ceiling on one blocked write: a request body is small, and a write that
/// blocks this long is a dead endpoint. A *ceiling*, not a schedule — the write
/// phase sets the socket's own write timeout to the smaller of this and what is
/// left of the call's deadline before every syscall ([`write_bounded`]), so a
/// stalled endpoint cannot hold an actor's thread, or a human's Stop, past the
/// call.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// A response body larger than this is refused while it is being read. With
/// [`MAX_HEAD_BYTES`] this is the whole of what one reply may make mush
/// allocate, so a server cannot make mush allocate without bound (docs §8).
/// Generous on purpose: a big diff or a long model reply is normal work. How
/// an over-cap reply is *said* depends on which framing carried the size: a
/// `Content-Length`, or a body that ended with the stream, is the answer's own
/// size and a plain refusal, while a chunk-size line past the cap is the
/// framing itself ([`Framing`], finding A10).
const MAX_BODY_BYTES: usize = 80 * 1024 * 1024;
/// The most bytes of a response *head* — the status line, every header line,
/// the head as a whole, and the chunk-size lines that frame a body — mush will
/// read. One named number for all of them, because the thing being bounded is
/// one: the memory a reply that has not framed itself may take from mush. A
/// head is small by every protocol on earth (a status line and a handful of
/// headers), so 64 KiB is generous; an endpoint that writes a line or a head
/// past it is one that will never send the newline, and the read that used to
/// grow a `Vec` there is the one the OOM killer took the whole process for,
/// every agent's transcript with it (finding A1). A head past the bound is a
/// refusal, not a framing error: the endpoint is the one that sent it, and the
/// human is owed its name and the size. [`MAX_BODY_BYTES`] is the same decision
/// one layer down; the two together are what makes "a server cannot make mush
/// allocate without bound" true.
const MAX_HEAD_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

/// Anything the client can read from and write to: a plain TCP stream or a
/// rustls TLS stream. `Box<dyn Read + Write>` is not a valid trait object, so
/// one supertrait is needed. `Send` because a kept connection is parked in the
/// pool a whole tree of actors shares.
///
/// Beside reading and writing it says one thing: how to bound a write. A
/// socket's own timeout is the only bound that can end a write the kernel is
/// holding, and it must be *this* call's remainder — a kept connection was
/// opened under an earlier ask's budget — so the write phase sets it through
/// this method on whatever connection the pool handed over. A stream with no
/// syscall to bound (a test's in-memory connection) does nothing, which is
/// exactly what it has to bound.
trait ReadWrite: Read + Write + Send {
    fn set_write_timeout(&self, _bound: Duration) -> io::Result<()> {
        Ok(())
    }
}

impl ReadWrite for TcpStream {
    fn set_write_timeout(&self, bound: Duration) -> io::Result<()> {
        TcpStream::set_write_timeout(self, Some(bound))
    }
}

impl ReadWrite for rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
    fn set_write_timeout(&self, bound: Duration) -> io::Result<()> {
        self.sock.set_write_timeout(Some(bound))
    }
}

pub fn get_json(url: &str, api_key: Option<&str>, timeout: Duration) -> io::Result<Response> {
    request(
        &Ask {
            method: "GET",
            url,
            body: None,
            api_key,
            timeout,
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
///
/// `timeout` is what is left of the logical call's deadline — `model.rs`'s
/// `retrying` owns the one deadline and hands each attempt its remainder — so
/// one ask can never spend more than the one deadline it was promised.
pub fn post_json(
    url: &str,
    body: &str,
    api_key: Option<&str>,
    cancel: &AtomicBool,
    timeout: Duration,
) -> io::Result<Response> {
    request(
        &Ask {
            method: "POST",
            url,
            body: Some(body),
            api_key,
            timeout,
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
    /// What is left of the logical call's deadline: the whole budget for this
    /// ask, not a per-read timeout. `post_json` is handed the remainder
    /// `model.rs`'s `retrying` keeps; `get_json` is one call with no retry of
    /// its own and passes its own whole deadline.
    timeout: Duration,
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
/// be handed a connection that failed. The pool never replaces a dead
/// connection itself: replacing one means writing the request again, and only
/// the layer that can prove no whole request was ever written may do that
/// ([`Unsent`]); a connection that dies after the request was written is final
/// (finding A2).
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
/// provable with no socket and no server. It is handed the call's [`Watch`],
/// because the phases before a request exists — the name lookup, the connect,
/// the TLS handshake — share the deadline the request itself is bounded by,
/// and a failure those phases cause by spending it is the watch's own answer.
type Open<'a> = &'a mut dyn FnMut(&str, u16, bool, &Watch<'_>) -> io::Result<Box<dyn ReadWrite>>;

fn request(ask: &Ask<'_>, clock: &dyn Clock, pool: &Pool, open: Open<'_>) -> io::Result<Response> {
    let watch = Watch::new(ask.cancel, ask.timeout, clock);
    // A Stop that arrived before the request did: do not pay for a call the
    // human already cancelled. A deadline that has passed before the first
    // byte is the same kind of decision — the call is over, and it is not a
    // retry's to make.
    watch.check()?;

    let (host, port, path, tls) = parse_url(ask.url)?;
    let endpoint = Endpoint {
        host: host.clone(),
        port,
        tls,
    };

    // The connection the last request to this endpoint left behind, or a fresh
    // one. Opening is the one step that happens before a request exists, so
    // everything it can fail with — a name that does not resolve, a connect
    // that is refused or times out, a TLS handshake — is `Unsent`: no byte of
    // the request was written, and asking again cannot duplicate or bill
    // anything (finding A2). The one exception is a call already *over*: a
    // Stop or a deadline a phase spent is the watch's own answer, already in
    // the road's words, and marking it `Unsent` would ask a call with nothing
    // left to ask with.
    let stream = match pool.take(&endpoint) {
        Some(stream) => stream,
        None => BufReader::new(open(&host, port, tls, &watch).map_err(|error| {
            if watch.spent() {
                error
            } else {
                unsent(error)
            }
        })?),
    };

    // One request, one reply, one connection: no second send hides in here.
    // Every failure after the write is final, whatever it is, because the
    // endpoint was handed the whole request and may already have read, run and
    // charged for it — only the layer that can prove nothing was written may
    // ask again (finding A2). The `?` drops the connection with the error, so
    // a failed exchange is never handed to the next request.
    let (response, stream, reusable) = exchange(stream, ask, &host, port, &path, &watch)?;
    if reusable {
        pool.keep(endpoint, stream);
    }
    Ok(response)
}

/// One request and its reply on one connection.
///
/// `Ok` hands the connection back so the caller can keep it, with whether the
/// reply framed itself well enough to be worth keeping. An `Err` is final for
/// every failure after the request was written: the endpoint may already have
/// received it, and mush never sends a request the endpoint may have seen
/// twice (finding A2). The one failure that is not final is the write itself,
/// which hands over no whole request and carries [`Unsent`] so `model.rs`
/// knows a repeat cannot duplicate anything — and that is *only* the shape it
/// takes while the call has time left: a write that spent the call's deadline
/// is the deadline's own answer, never a request to ask again.
///
/// A `Response` exists only for a reply whose body framed itself completely
/// (to the `Content-Length`, through the zero chunk and its trailer, or to the
/// stream's end). Every `Err` is therefore a reply of which *nothing was handed
/// over* — the wire broke, the body was cut off, a frame did not parse — and no
/// partial answer can ever be mistaken for the caller's reply.
fn exchange(
    mut stream: Socket,
    ask: &Ask<'_>,
    host: &str,
    port: u16,
    path: &str,
    watch: &Watch,
) -> io::Result<(Response, Socket, bool)> {
    if let Err(error) = write_request(&mut stream, ask, host, port, path, watch) {
        // The write did not hand the whole request over: no complete request
        // ever reached the endpoint, so this is the one failure a repeat cannot
        // duplicate — marked `Unsent`, the class `model.rs::retrying` asks
        // again (finding A2). A cancellation is the human's own decision, and a
        // deadline the write spent is the call's: both are reported as
        // themselves, never as a request to ask again with nothing left.
        return Err(
            if error.kind() == io::ErrorKind::Interrupted || watch.spent() {
                error
            } else {
                unsent(error)
            },
        );
    }

    // A blank line is not an answer: a kept connection can carry one from the
    // exchange before it (a server's keep-alive probe, or framing that left a
    // CRLF behind — see `read_chunked`). Read as the status line it was
    // reported as `malformed status line: ""`, refusing a reply that had not
    // even started.
    // What the head has cost so far: each line is bounded by [`MAX_HEAD_BYTES`],
    // and this is the head's own share of the same number, so an endpoint that
    // sends short lines forever cannot sit in mush's memory either. Each line
    // is counted with its terminator (two bytes), so the guard can only close
    // early, never late.
    let mut head_bytes = 0usize;
    let status_line = loop {
        match read_line(&mut stream, watch) {
            Ok(Some(line)) if line.is_empty() => continue,
            Err(error) if is_overlong(&error) => return Err(head_too_large(ask.url)),
            Ok(Some(line)) => {
                head_bytes += line.len() + 2;
                if head_bytes > MAX_HEAD_BYTES {
                    return Err(head_too_large(ask.url));
                }
                break line;
            }
            // Nothing at all came back: the peer closed the connection before it
            // answered (a kept connection the server has since dropped), which is
            // not the same thing as a malformed status line, and must not be
            // reported as one.
            Ok(None) => {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "the connection ended before it answered",
                ))
            }
            Err(error) => return Err(error),
        }
    };
    let status = parse_status(&status_line)?;

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    let mut close = false;
    loop {
        let line = match read_line(&mut stream, watch) {
            Ok(Some(line)) => line,
            // The headers ended at the stream's end: the framing they would
            // have given is simply absent, exactly as it was before.
            Ok(None) => break,
            Err(error) if is_overlong(&error) => return Err(head_too_large(ask.url)),
            Err(error) => return Err(error),
        };
        head_bytes += line.len() + 2;
        if head_bytes > MAX_HEAD_BYTES {
            return Err(head_too_large(ask.url));
        }
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
                    return Err(framing(format!(
                        "malformed Content-Length: {:?}",
                        value.trim()
                    )))
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
    let body = body?;

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
    // asking the watch first means a Stop still wins over the retry. Every
    // write is bounded by the smaller of [`WRITE_TIMEOUT`] and what is left of
    // the call, per syscall ([`write_bounded`]).
    let out = stream.get_mut();
    write_bounded(&mut **out, head.as_bytes(), watch)?;
    if let Some(body) = ask.body {
        write_bounded(&mut **out, body.as_bytes(), watch)?;
    }
    flush_bounded(&mut **out, watch)
}

/// Write `bytes` whole, and let nothing outlive the call: before every chunk
/// the watch is consulted and the socket's own write timeout is set to the
/// smaller of [`WRITE_TIMEOUT`] and what is left — so a syscall the kernel
/// holds cannot block past the deadline, however many chunks were accepted
/// before it. The bound is set per chunk and per ask, never once per
/// connection: a kept connection carries the ask that opened it, and this write
/// must be bounded by *this* ask's remainder.
///
/// A timeout whose bound was the call's own remainder is the call being over —
/// said through [`Watch::spend`] in the road's words — never the write's own
/// wire failure, which the caller would mark [`Unsent`] and `model.rs` would
/// ask again with nothing left to ask with.
fn write_bounded(out: &mut dyn ReadWrite, bytes: &[u8], watch: &Watch) -> io::Result<()> {
    let mut rest = bytes;
    while !rest.is_empty() {
        watch.check()?;
        let left = watch.left();
        let bound = WRITE_TIMEOUT.min(left);
        out.set_write_timeout(bound)?;
        match retrying_interrupted(Some(watch), || out.write(rest)) {
            // A peer that took none of the bytes it was offered is the wire
            // failing, not a signal: the error `write_all` would have raised.
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "the endpoint accepted no more of the request",
                ))
            }
            Ok(taken) => rest = &rest[taken..],
            Err(error) if is_timeout(&error) && bound == left => return Err(watch.spend()),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// One `flush`, bounded and answered like a write chunk: on a TLS stream a
/// flush *is* a write, so it takes the same rule rather than whatever bound the
/// last chunk happened to leave on the socket.
fn flush_bounded(out: &mut dyn ReadWrite, watch: &Watch) -> io::Result<()> {
    watch.check()?;
    let left = watch.left();
    let bound = WRITE_TIMEOUT.min(left);
    out.set_write_timeout(bound)?;
    match retrying_interrupted(Some(watch), || out.flush()) {
        Err(error) if is_timeout(&error) && bound == left => Err(watch.spend()),
        result => result,
    }
}

/// A cancellation flag and a deadline, threaded through one request's reads.
///
/// The socket is given [`READ_SLICE`] as its read timeout, so every read wakes
/// up quickly; `check` is what turns those wake-ups into a decision — keep
/// waiting, or stop because the human asked us to.
///
/// The deadline is the *call's*, not a phase's, and this is where the rule that
/// keeps it that way has its one home. `model.rs`'s `retrying` owns the logical
/// call and hands each attempt only what is left of its one deadline;
/// everything an attempt does — the name lookup, the connect, the write, every
/// read — must fit inside what it was handed. So each phase takes the *smaller*
/// of its own ceiling ([`CONNECT_TIMEOUT`], [`WRITE_TIMEOUT`],
/// [`RESOLVE_TIMEOUT`]; [`READ_SLICE`] is polled between reads inside the
/// deadline) and [`left`](Self::left), and a phase that runs out of the budget
/// says so through [`spend`](Self::spend) — the sentence the deadline itself
/// uses, so `model.rs` classifies it as the deadline it already is and never as
/// a wire failure it may ask again. A phase that kept its own schedule instead
/// would extend the call it serves: a stalled write alone could hold an actor's
/// thread, and a human's Stop, thirty seconds past a deadline with a second
/// left.
struct Watch<'a> {
    cancel: Option<&'a AtomicBool>,
    deadline: Instant,
    /// The clock the deadline is compared against. Real requests read the
    /// system one; a test reaches a deadline by advancing a fake, so the
    /// "the endpoint stopped responding" path does not cost the suite the
    /// timeout it is proving.
    clock: &'a dyn Clock,
    /// Set the moment a phase spends the call — a Stop that landed or the
    /// deadline reached — so the roads that wrap a phase's failure in
    /// `Unsent` can tell "the call is over" from "the request never left",
    /// without a second marker class beside [`Unsent`] for it.
    spent: Cell<bool>,
}

/// The human's Stop, in the road's own words — what a phase that reads the flag
/// answers with, wherever it reads it.
fn stopped() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "request cancelled")
}

/// The call's budget running out, in the road's own words: the sentence
/// [`Watch`]'s read deadline already used, said by every phase that spends it,
/// so the layer above classifies a spent phase as the deadline it is (finding
/// A2) rather than as a new kind of failure.
fn expired() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "the endpoint stopped responding")
}

impl<'a> Watch<'a> {
    fn new(cancel: Option<&'a AtomicBool>, timeout: Duration, clock: &'a dyn Clock) -> Self {
        Self {
            cancel,
            deadline: clock.now() + timeout,
            clock,
            spent: Cell::new(false),
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel
            .map(|flag| flag.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    /// Whether the call's deadline has been reached: `check`'s clock half, for
    /// a phase that must name its own ceiling when the ceiling — not the call —
    /// is what ended it.
    fn at_deadline(&self) -> bool {
        self.clock.now() >= self.deadline
    }

    /// What is left of the call's deadline: the budget every phase gets, which
    /// each bounds by the smaller of its own ceiling and this.
    fn left(&self) -> Duration {
        self.deadline.saturating_duration_since(self.clock.now())
    }

    /// What a phase that has spent the call is told: the human's Stop first,
    /// then the deadline, in the road's own words — and the fact is marked, so
    /// every road below passes this through instead of wrapping it in the
    /// repeatable [`Unsent`].
    fn spend(&self) -> io::Error {
        self.spent.set(true);
        if self.cancelled() {
            stopped()
        } else {
            expired()
        }
    }

    /// Whether a phase has already spent the call. Read by the two roads that
    /// mark a pre-reply failure `Unsent` before they know whether the call is
    /// over.
    fn spent(&self) -> bool {
        self.spent.get()
    }

    /// Consulted after every successful read *and* on every read timeout: a
    /// cancellation or a deadline has to be able to stop a body that keeps
    /// arriving in slices, not only a silent one.
    fn check(&self) -> io::Result<()> {
        if self.cancelled() {
            return Err(self.spend());
        }
        if self.at_deadline() {
            return Err(self.spend());
        }
        Ok(())
    }
}

/// A call that ran out of its own time bound rather than failing: the read
/// slice, a connect attempt, a blocked write. Those are the errors a phase
/// answers with [`Watch::spend`] when the call's remainder was the bound.
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
/// `watch` is the call behind the operation — the request's reads and writes,
/// the connect, and the name-lookup wait all carry it, so a Stop or a spent
/// deadline is returned as itself rather than retried after. Only a call with
/// no request behind it at all passes `None`: the lookup that runs on the
/// resolver's own thread, where there is no flag to read.
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

/// Read one line, refusing one longer than [`MAX_HEAD_BYTES`].
///
/// Every line mush reads off a reply is a line of the head or of the framing
/// that ends one — a status line, a header line, a chunk-size line, a trailer
/// field — and before the body there is nothing to bound one: an endpoint that
/// never writes the newline used to make this `Vec` grow until the process was
/// OOM-killed (finding A1). The bound is the head's own number; a chunk-size
/// line is read by the same reader for the same reason, and the caller that
/// knows which line it was refuses it as what it is ([`OverlongLine`]).
///
/// Returns the line without its terminator, or `None` at end of stream.
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
        if line.len() + take > MAX_HEAD_BYTES {
            return Err(overlong_line());
        }
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

/// The refusal a body past [`MAX_BODY_BYTES`] gets when the size is the
/// *answer's own*: the number in a `Content-Length`, or the bytes that arrived
/// before the stream ended. Plain `InvalidData` — a refusal, never [`Framing`]
/// — because the endpoint framed a body that size and mush is the one saying
/// no. A chunk-size line past the cap is the framing's own claim, not the
/// answer's size, and [`read_chunked`] raises [`framing`] for it before this is
/// reached (finding A10).
fn body_too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("the response body is larger than {MAX_BODY_BYTES} bytes"),
    )
}

/// Connect to the endpoint. Every step here is bounded by the *call's* deadline
/// before its own ceiling: the name lookup by [`RESOLVE_TIMEOUT`], the TCP
/// connect by [`CONNECT_TIMEOUT`], the TLS handshake by what is left, and every
/// read by [`READ_SLICE`] with the watch between the slices. [`Watch`] owns the
/// deadline the phases share, and a phase that runs out of it answers in the
/// watch's own voice — never as a wire failure (finding A19; finding A2's one
/// deadline).
fn connect(host: &str, port: u16, tls: bool, watch: &Watch<'_>) -> io::Result<Box<dyn ReadWrite>> {
    let mut last_error = None;
    // The lookup is the call's first phase: `resolve_bounded` ends its wait at
    // the smaller of its own ceiling and what is left of the call.
    let name = host.to_string();
    let addresses = resolve_bounded(host, port, watch, move || {
        retrying_interrupted(None, || (name.as_str(), port).to_socket_addrs())
            .map(|addresses| addresses.collect())
    })?;
    for address in addresses {
        // What is left *now*, not when the call began: a second address must
        // not get a fresh ceiling after the first spent the budget. A connect
        // that ran out on the budget is the call being over, said the same way
        // as every other spent phase; a connect that ran out on its own
        // ceiling is the wire failing, still `Unsent` while the call has time.
        let left = watch.left();
        if left.is_zero() {
            return Err(watch.spend());
        }
        let bound = CONNECT_TIMEOUT.min(left);
        let stream =
            match retrying_interrupted(Some(watch), || TcpStream::connect_timeout(&address, bound))
            {
                Ok(stream) => stream,
                Err(error) => {
                    if is_timeout(&error) && bound == left {
                        return Err(watch.spend());
                    }
                    last_error = Some(error);
                    continue;
                }
            };
        // Liveness guards, not UX timers: a stalled endpoint must not pin a
        // thread (and, for the model list, the whole TUI) forever. Each is the
        // smaller of its ceiling and the call's remainder; the write phase
        // sets its own bound again, per request, because a kept connection
        // carries the ask that opened it. A connect that used the last of the
        // budget is the call being over like any other spent phase.
        let left = watch.left();
        if left.is_zero() {
            return Err(watch.spend());
        }
        stream.set_write_timeout(Some(WRITE_TIMEOUT.min(left)))?;
        if tls {
            // A TLS handshake is a conversation, not a read, so during setup it
            // gets the whole remainder; only the reads after it get the short
            // slice that lets a cancellation land while the model thinks. A
            // handshake that spends the remainder is the call being over.
            stream.set_read_timeout(Some(left))?;
            let stream = match tls_connect(host, stream) {
                Ok(stream) => stream,
                Err(error) if is_timeout(&error) => return Err(watch.spend()),
                Err(error) => return Err(error),
            };
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

/// The ceiling on the wait for a name. std's `to_socket_addrs` cannot be given
/// a timeout, and it is the step of `connect` that could hang past every
/// deadline mush sets (a dead DNS server, a wedged VPN) — so the lookup runs on
/// its own thread and this is the ceiling the caller waits on (finding A19). A
/// *ceiling*, not a schedule: the wait ends at the smaller of this and what is
/// left of the call's deadline.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);
/// The resolver wait loop's slice: short enough that a deadline or a shutdown
/// is noticed promptly, long enough not to spin.
const RESOLVE_SLICE: Duration = Duration::from_millis(50);

/// Resolve `host:port`, bounded by the smaller of [`RESOLVE_TIMEOUT`] and what
/// is left of the call's deadline ([`Watch`]).
///
/// The lookup itself still blocks on its own thread; what is bounded is the
/// *wait* for it. A lookup that outlives its bound is abandoned, not killed —
/// the resolver thread belongs to the OS to collect. Which bound ended the wait
/// decides the answer: the call's own deadline answers in the watch's voice
/// (the same sentence every spent phase uses), while [`RESOLVE_TIMEOUT`] alone
/// gets a `TimedOut` naming the host and the seconds, a fact the caller can
/// report instead of hanging. The clock is the watch's, so a test reaches a
/// deadline by advancing a fake instead of waiting ten seconds (finding A19).
fn resolve_bounded(
    host: &str,
    port: u16,
    watch: &Watch<'_>,
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
    let deadline = watch.deadline.min(watch.clock.now() + RESOLVE_TIMEOUT);
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
        // A Stop lands wherever the flag can be read, and this wait can read
        // it between slices.
        if watch.cancelled() {
            return Err(watch.spend());
        }
        if watch.clock.now() >= deadline {
            return Err(if watch.at_deadline() {
                // The call's budget, not this phase's ceiling, is what ran
                // out: it is the deadline's answer, in the deadline's words.
                watch.spend()
            } else {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "{host}:{port} did not resolve within {}s",
                        RESOLVE_TIMEOUT.as_secs()
                    ),
                )
            });
        }
        watch.clock.sleep(RESOLVE_SLICE);
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

/// A request that never left mush: the endpoint could not be dialled at all
/// (a name that does not resolve, a connect that is refused or times out, a
/// TLS handshake that fails), or the write failed before the request was whole.
/// No complete request ever arrived, so the endpoint has nothing to have read,
/// run or charged for — the one failure class a repeat cannot duplicate, and
/// the reason it is marked rather than derived from the error kind: a
/// `ConnectionReset` while writing means the request was not whole, while one
/// while reading means it may already have been answered (finding A2).
#[derive(Debug)]
struct Unsent(String);

impl fmt::Display for Unsent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unsent {}

/// `error`, marked as a request that never left mush, keeping its kind so a
/// caller that reports the wire can still say what happened.
fn unsent(error: io::Error) -> io::Error {
    io::Error::new(error.kind(), Unsent(error.to_string()))
}

/// Whether `error` is a request that never left mush — [`Unsent`] as
/// [`is_framing`] is [`Framing`]. `model.rs` asks this to decide the one
/// failure a repeat may ask again.
pub fn is_unsent(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.downcast_ref::<Unsent>().is_some())
}

/// A reply whose framing broke before its body could be read: a status line or
/// `Content-Length` that is not one, a chunk size that is not hex or that
/// claims past [`MAX_BODY_BYTES`], a chunk terminator that is not the
/// terminator the framing promised.
///
/// Its own type rather than a bare `InvalidData`, because the kind alone cannot
/// say whether the endpoint *answered* or its reply *broke on the way in* — and
/// the two want opposite treatment. A body whose `Content-Length`, or whose
/// stream's end, put it past [`MAX_BODY_BYTES`] is an answer mush refuses (a
/// `Refused`, never retried). A chunk-size line that claims past the cap is
/// framing instead, because that line *is* the framing of a body mush has not
/// read — a garbled `FFFFFFFF` and an honest 4 GiB chunk are the same bytes to
/// mush, so it reports the claim, not the endpoint's answer (finding A10).
/// Either way the frame was never handed to the caller,
/// so it is reported as what it is — bytes that failed to frame themselves,
/// not the endpoint's opinion of the request — and the connection that carried
/// it is dropped rather than kept (finding B27, `model.rs`).
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

/// A line that reached [`MAX_HEAD_BYTES`] without its terminator: the read that
/// has no end to stop at, and the shape an endpoint with no newline to send
/// leaves behind. Its own marker rather than a message because the two roads
/// that read lines refuse it as different things — a response head past the
/// bound is an answer mush refuses, while a chunk-size line past it is framing
/// that never parsed — and only the caller that read the line knows which.
#[derive(Debug)]
struct OverlongLine;

impl fmt::Display for OverlongLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a reply line that never ended")
    }
}

impl std::error::Error for OverlongLine {}

fn overlong_line() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, OverlongLine)
}

fn is_overlong(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.downcast_ref::<OverlongLine>().is_some())
}

/// The refusal a response head past [`MAX_HEAD_BYTES`] gets: the endpoint and
/// the bound in one sentence, because the head is the endpoint's to send and
/// the human is the one who can act on it. A plain `InvalidData` — a refusal,
/// the class of a body past [`MAX_BODY_BYTES`] on the `Content-Length` and
/// end-of-stream roads, never [`Framing`]: the endpoint answered, and mush is
/// the one saying no. The connection is dropped with the error, so nothing of
/// the oversized head waits in the pool.
fn head_too_large(endpoint: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{endpoint} sent a response head larger than {MAX_HEAD_BYTES} bytes"),
    )
}

/// The same line on the body road: a chunk-size or trailer line that never
/// ended is framing that never parsed, and says so in [`Framing`]'s own words
/// rather than as an answer mush refuses.
fn overlong_framing() -> io::Error {
    framing(format!(
        "a chunked-framing line larger than {MAX_HEAD_BYTES} bytes"
    ))
}

/// Exactly `len` bytes of a chunked body, where the stream ending early is the
/// body being cut off rather than `read_exact`'s `Content-Length` complaint.
/// `len` has already been held to [`MAX_BODY_BYTES`] by [`read_chunked`], so
/// the cap `read_exact` would raise is not this road's answer.
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
            match read_line(reader, watch) {
                Ok(Some(line)) if line.trim().is_empty() => continue,
                Ok(Some(line)) => break line,
                Ok(None) => return Err(body_cut_off()),
                Err(error) if is_overlong(&error) => return Err(overlong_framing()),
                Err(error) => return Err(error),
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
                match read_line(reader, watch) {
                    Ok(Some(line)) if line.is_empty() => break,
                    // A trailer field: part of this body, not the next reply.
                    Ok(Some(_)) => continue,
                    // The stream ended at the zero chunk; the body is complete.
                    Ok(None) => break,
                    Err(error) if is_overlong(&error) => return Err(overlong_framing()),
                    Err(error) => return Err(error),
                }
            }
            break;
        }
        // A chunk-size line *is* the framing, so a size past the cap is a reply
        // that broke where mush reads it — never an answer mush refuses.
        // `FFFFFFFF` is what a garbled line parses to, and it is
        // indistinguishable from an honest 4 GiB chunk: mush only ever has the
        // claim, so it reports the claim (with the size and the cap in the
        // words) rather than `body_too_large()`'s sentence about a body the
        // endpoint never handed over (finding A10).
        if out.len() + size > MAX_BODY_BYTES {
            return Err(framing(format!(
                "a chunk size of {size} bytes would put the body past the {MAX_BODY_BYTES}-byte body cap"
            )));
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
    use crate::model::CHAT_DEADLINE;
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
        let error = post_json(&url, "{}", None, &cancel, Duration::from_secs(5)).unwrap_err();
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
    /// on its own thread and the wait is bounded by the watch's clock, so
    /// advancing a fake is the whole test. The budget is past the resolver's
    /// *own* ceiling, so that ceiling is what ends this wait and the answer
    /// names the host. Before this, when the wait ended was the OS resolver's to
    /// decide (finding A19).
    #[test]
    fn a_resolver_that_never_answers_is_bounded_by_the_clock() {
        let clock = Advanceable::new();
        let cancel = AtomicBool::new(false);
        let watch = Watch::new(
            Some(&cancel),
            RESOLVE_TIMEOUT + Duration::from_secs(60),
            &clock,
        );
        let started = Instant::now();
        let error = resolve_bounded("mush.invalid", 80, &watch, || {
            std::thread::sleep(Duration::from_secs(3600));
            Err(io::Error::other("too late"))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert!(error.to_string().contains("mush.invalid"), "{error}");
        assert!(
            clock.elapsed() >= RESOLVE_TIMEOUT,
            "the ceiling is what ended it: {:?}",
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
        let cancel = AtomicBool::new(false);
        let watch = Watch::new(Some(&cancel), Duration::from_secs(1), clock::system());
        let addresses = resolve_bounded("127.0.0.1", 1, &watch, move || Ok(vec![address])).unwrap();
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
            Duration::from_secs(5),
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

    /// An in-memory connection has no syscall to bound: the default
    /// `set_write_timeout` is what the write phase's rule reduces to here.
    impl ReadWrite for Wire {}

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
            timeout: Duration::from_secs(5),
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
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _watch: &Watch<'_>| {
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

    /// A kept connection the server has since closed, whose request was written
    /// onto it anyway: nothing comes back, and the call is **final**. The write
    /// succeeded, so the endpoint may already have received the request, and
    /// mush does not quietly write it a second time (finding A2). The next call
    /// opens its own connection, and the failed one is not kept.
    #[test]
    fn a_kept_connection_that_died_after_the_write_is_final() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let opened = Arc::new(AtomicUsize::new(0));
        let opens = opened.clone();
        let mut queue = std::collections::VecDeque::new();
        // Serves one reply, then ends the stream (the server hung up).
        queue.push_back(wire(&written, &[&ok("{\"one\":1}")]));
        // The connection opened after that answers normally.
        queue.push_back(wire(&written, &[&ok("{\"two\":2}")]));
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _watch: &Watch<'_>| {
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
        let second = send(&pool, &mut opener, url, "{}").unwrap_err();
        assert_eq!(second.kind(), io::ErrorKind::ConnectionAborted, "{second}");
        assert_eq!(
            second.to_string(),
            "the connection ended before it answered",
            "the wire failing, not a parse error the endpoint sent: {second}"
        );
        assert_eq!(
            opened.load(Ordering::SeqCst),
            1,
            "no replacement: the request may already have been received"
        );
        assert_eq!(pool.idle(), 0, "the failed connection is never kept");

        // And the pool is still usable: the next call opens its own.
        assert_eq!(
            send(&pool, &mut opener, url, "{}").unwrap().body,
            "{\"two\":2}"
        );
        assert_eq!(opened.load(Ordering::SeqCst), 2);
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
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _watch: &Watch<'_>| {
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
    /// never kept, so the next request opens a fresh connection and cannot read
    /// leftover framing as its own reply.
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
        // The connection opened after it — the next call — answers normally.
        queue.push_back(wire(&written, &[&ok("{\"after\":1}")]));
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _watch: &Watch<'_>| {
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
            "the next call opens a fresh connection, never the broken one"
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
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _watch: &Watch<'_>| {
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
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _watch: &Watch<'_>| match queue
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

    /// The classification apart from the policy: a frame that did not parse is
    /// a *framing* error — the reply broke on the way in, nothing of it was
    /// handed over — while a body or head past a cap is an answer mush refuses.
    /// The kind is `InvalidData` for both, which is exactly why the marker
    /// exists.
    #[test]
    fn a_frame_that_did_not_parse_is_marked_apart_from_a_refusal() {
        let huge = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        let head_over = format!(
            "HTTP/1.1 200 OK\r\nX-Big: {}\r\n",
            "a".repeat(MAX_HEAD_BYTES)
        );
        let cases: [(&str, bool); 5] = [
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
            // A response head past its bound, too: the endpoint really sent it,
            // so it is an answer mush refuses rather than a frame that broke.
            (&head_over, false),
        ];
        for (answer, framed) in cases {
            let written = Arc::new(Mutex::new(Vec::new()));
            let mut queue = std::collections::VecDeque::new();
            queue.push_back(wire(&written, &[answer]));
            let mut opener =
                move |_host: &str, _port: u16, _tls: bool, _watch: &Watch<'_>| match queue
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

    /// A chunk-size line *is* the framing, so a size that claims past
    /// [`MAX_BODY_BYTES`] is a reply breaking where mush reads it — never the
    /// endpoint's refusal. A garbled line that still parses as hex
    /// (`FFFFFFFF`, just under 4 GiB) is indistinguishable from an honest
    /// chunk that size, and mush only ever has the claim: `body_too_large()`
    /// said the endpoint had sent a body of 83886080 bytes, blaming a healthy
    /// endpoint for a flaky proxy's line (finding A10). The `Content-Length`
    /// half stays a refusal, pinned by `an_oversized_body_is_refused`.
    #[test]
    fn a_chunk_size_claim_past_the_cap_is_framing_not_the_endpoints_refusal() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(wire(
            &written,
            &["HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nFFFFFFFF\r\nhello\r\n0\r\n\r\n"],
        ));
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _watch: &Watch<'_>| match queue
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
        assert!(is_framing(&error), "a chunk-size claim is framing: {error}");
        assert_eq!(
            error.to_string(),
            format!(
                "a chunk size of 4294967295 bytes would put the body past the {MAX_BODY_BYTES}-byte body cap"
            ),
            "the claim names the size it read and the cap it passed"
        );
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
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _watch: &Watch<'_>| {
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
            timeout: Duration::from_secs(5),
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

    impl ReadWrite for Drip {}

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
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _watch: &Watch<'_>| {
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
    /// when a server probes an idle connection and closes it). It must not be
    /// read as a malformed status line: the call ends as the connection that
    /// ended before it answered — and it is final, because the request was
    /// written on it first (finding A2).
    #[test]
    fn a_blank_line_then_a_closed_connection_is_not_a_parse_error() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let opened = Arc::new(AtomicUsize::new(0));
        let opens = opened.clone();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(wire(&written, &[&ok("{\"one\":1}"), "\r\n"]));
        queue.push_back(wire(&written, &[&ok("{\"two\":2}")]));
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _watch: &Watch<'_>| {
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
        let second = send(&pool, &mut opener, url, "{}").unwrap_err();
        assert_eq!(second.kind(), io::ErrorKind::ConnectionAborted, "{second}");
        assert!(
            !is_framing(&second),
            "a closed connection is not a frame that did not parse: {second}"
        );
        assert_eq!(
            opened.load(Ordering::SeqCst),
            1,
            "the request was written, so it is not sent again"
        );
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

    impl ReadWrite for Interrupted {}

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
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _watch: &Watch<'_>| {
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
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _watch: &Watch<'_>| {
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
            timeout: Duration::from_secs(5),
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
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _watch: &Watch<'_>| {
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
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _watch: &Watch<'_>| {
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
        let mut opener = move |_host: &str, _port: u16, _tls: bool, _watch: &Watch<'_>| {
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
            "no second send hides inside one request"
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
        let error = post_json(&url, "{}", None, &cancel, Duration::from_secs(5)).unwrap_err();
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
    /// the framing is — and this is the half that stays a refusal: the size is
    /// the answer's own, written in the endpoint's `Content-Length`, so the
    /// endpoint is the one that framed a body that large and mush is the one
    /// saying no. The chunked half is *not* a refusal — a chunk-size line is
    /// the framing — and is pinned by
    /// `a_chunk_size_claim_past_the_cap_is_framing_not_the_endpoints_refusal`
    /// (finding A10).
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
                // An announced length past the cap, with no body behind it:
                // the refusal is the number's, so the read stops before a byte
                // of the body has to arrive.
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
        assert!(
            !is_framing(&error),
            "the endpoint's own size is a refusal, not framing: {error}"
        );
        assert_eq!(
            error.to_string(),
            format!("the response body is larger than {MAX_BODY_BYTES} bytes"),
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// A response *head* is bounded, not only a body: an endpoint that writes a
    /// header line with no newline in it used to make `read_line` grow a `Vec`
    /// until the OOM killer took the process — a probe's 85 MiB head cost
    /// 178 MB of peak RSS and the call still returned `200` (finding A1). The
    /// refusal names the endpoint and the size, and the connection is dropped
    /// at once: the endpoint's own writes stop at the bound instead of an
    /// 85 MiB line.
    #[test]
    fn a_header_line_past_the_bound_is_a_refusal() {
        use std::io::Write as _;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let written = Arc::new(AtomicUsize::new(0));
        let counter = written.clone();
        let server = std::thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            let mut scratch = [0u8; 1024];
            let _ = connection.read(&mut scratch);
            // A status line, and then a header line with no newline in it: the
            // shape that used to grow until the process died.
            let _ = connection.write_all(b"HTTP/1.1 200 OK\r\nX-Big: ");
            let block = vec![b'a'; 64 * 1024];
            while counter.load(Ordering::SeqCst) < 256 * MAX_HEAD_BYTES {
                match connection.write(&block) {
                    Ok(0) | Err(_) => return true,
                    Ok(n) => {
                        counter.fetch_add(n, Ordering::SeqCst);
                    }
                }
            }
            // Not one write failed: is the connection at least gone?
            let _ = connection.set_read_timeout(Some(Duration::from_secs(2)));
            matches!(connection.read(&mut [0u8; 1]), Ok(0) | Err(_))
        });

        let url = format!("http://127.0.0.1:{port}/v1/models");
        let error = get_json(&url, None, Duration::from_secs(5)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
        assert!(
            !is_framing(&error),
            "a head past the bound is an answer mush refuses, not framing that broke: {error}"
        );
        let message = error.to_string();
        assert!(message.contains(&url), "the endpoint is named: {message}");
        assert!(
            message.contains(&MAX_HEAD_BYTES.to_string()),
            "the bound is named: {message}"
        );
        assert!(
            server.join().unwrap(),
            "the connection was not dropped after the refusal"
        );
        let bytes = written.load(Ordering::SeqCst);
        assert!(
            bytes < 64 * MAX_HEAD_BYTES,
            "the endpoint wrote {bytes} bytes before the close: an unbounded head let it\
             write the whole line"
        );
    }

    /// An endpoint that accepts the connection and then never reads it: the
    /// request's write stalls on the kernel's buffers. Held open long enough
    /// for the test that made it to outlive it.
    fn never_read_endpoint() -> u16 {
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

    /// An endpoint that reads the whole request and answers `body`: the healthy
    /// shape no phase bound may cut. The client sends the request in one go and
    /// then waits for its reply, so a read that pauses is the request being
    /// complete rather than a stall.
    fn answering_endpoint(body: &'static str) -> u16 {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut connection, _)) = listener.accept() {
                connection
                    .set_read_timeout(Some(Duration::from_millis(300)))
                    .unwrap();
                let mut scratch = [0u8; 64 * 1024];
                loop {
                    match connection.read(&mut scratch) {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(error) if is_timeout(&error) => break,
                        Err(_) => return,
                    }
                }
                let answer = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = connection.write_all(answer.as_bytes());
                let _ = connection.flush();
                // Long enough that the reply is read whole before the close.
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        port
    }

    /// No phase may extend the call's deadline. Each phase of the wire — the
    /// name lookup, the connect, the write — gets the smaller of its own
    /// ceiling and what is left of the call, so a phase that stalls ends the
    /// call at the call's deadline. Before this rule one ask could spend its
    /// deadline *plus* those ceilings — a stalled write alone held the actor's
    /// thread, and a human's Stop, for [`WRITE_TIMEOUT`].
    #[test]
    fn no_phase_outlives_the_calls_deadline() {
        let deadline = Duration::from_millis(300);

        // The name lookup. `connect` uses the OS resolver, with no seam for a
        // lookup that never answers, so the phase runs through the opener's own
        // [`Watch`] — exactly what `connect` hands `resolve_bounded` — on a fake
        // clock, where waiting out RESOLVE_TIMEOUT costs nothing.
        let clock = Advanceable::new();
        let ask = Ask {
            method: "POST",
            url: "http://resolver.test:80/v1/chat/completions",
            body: Some("{}"),
            api_key: None,
            timeout: deadline,
            cancel: None,
        };
        let mut opener = |_host: &str, _port: u16, _tls: bool, watch: &Watch<'_>| {
            resolve_bounded("resolver.test", 80, watch, || {
                std::thread::sleep(Duration::from_secs(3600));
                Err(io::Error::other("too late"))
            })?;
            unreachable!("the lookup never answers")
        };
        let error = request(&ask, &clock, &Pool::new(), &mut opener).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the endpoint stopped responding",
            "the resolve phase ends in the deadline's own voice: {error}"
        );
        assert!(
            !is_unsent(&error),
            "a spent deadline is the call's answer, not a request to ask again: {error}"
        );
        assert_eq!(
            clock.elapsed(),
            deadline,
            "the lookup waited the call's budget, not RESOLVE_TIMEOUT"
        );

        // The connect: 10.255.255.1 is carried by the default route and nothing
        // answers it, so the SYN is swallowed and the connect stalls.
        let started = Instant::now();
        let error = post_json(
            "http://10.255.255.1:9/v1/chat/completions",
            "{}",
            None,
            &AtomicBool::new(false),
            deadline,
        )
        .unwrap_err();
        let elapsed = started.elapsed();
        assert_eq!(
            error.kind(),
            io::ErrorKind::TimedOut,
            "the connect's bound is the call's remainder: {error}"
        );
        assert!(
            !is_unsent(&error),
            "a spent budget is the deadline, not an unsent connect: {error}"
        );
        assert!(
            elapsed >= deadline && elapsed < Duration::from_secs(2),
            "not CONNECT_TIMEOUT past the deadline: {elapsed:?}"
        );

        // The write: the listener accepts and then never reads, so a body past
        // the loopback buffers stalls in the kernel. The write phase sets the
        // socket's own timeout to what is left of the call and checks the watch
        // between chunks, so the call ends at the deadline — never
        // WRITE_TIMEOUT past it — and ends as the deadline rather than as a
        // retryable request.
        let port = never_read_endpoint();
        let url = format!("http://127.0.0.1:{port}/v1/chat/completions");
        let body = "x".repeat(16 * 1024 * 1024);
        let started = Instant::now();
        let error = post_json(&url, &body, None, &AtomicBool::new(false), deadline).unwrap_err();
        let elapsed = started.elapsed();
        assert_eq!(
            error.kind(),
            io::ErrorKind::TimedOut,
            "the stalled write ends in the deadline's own voice: {error}"
        );
        assert!(
            !is_unsent(&error),
            "a spent budget is the deadline, not an unsent write: {error}"
        );
        assert!(
            elapsed >= deadline && elapsed < Duration::from_secs(2),
            "never WRITE_TIMEOUT past the deadline: {elapsed:?}"
        );
    }

    /// A Stop that lands while a write stalls is read as soon as the write can
    /// return, and the write returns at the call's deadline — not thirty seconds
    /// later at [`WRITE_TIMEOUT`]. The deadline is what ends the stall and the
    /// Stop is then the answer; this asserts the classification and the elapsed
    /// time rather than driving a real Ctrl-C, which the seam does not reach.
    #[test]
    fn a_stop_lands_while_a_write_stalls() {
        let cancel = Arc::new(AtomicBool::new(false));
        let setter = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            setter.store(true, Ordering::SeqCst);
        });

        let port = never_read_endpoint();
        let url = format!("http://127.0.0.1:{port}/v1/chat/completions");
        let body = "x".repeat(16 * 1024 * 1024);
        let started = Instant::now();
        let error = post_json(&url, &body, None, &cancel, Duration::from_millis(400)).unwrap_err();
        let elapsed = started.elapsed();
        assert_eq!(
            error.to_string(),
            "request cancelled",
            "the Stop is the answer, read as soon as the write returns: {error}"
        );
        assert_eq!(error.kind(), io::ErrorKind::Interrupted, "{error}");
        assert!(
            elapsed < Duration::from_secs(2),
            "the Stop did not wait out WRITE_TIMEOUT: {elapsed:?}"
        );
    }

    /// The other side of the rule: the constants are ceilings, not schedules. A
    /// healthy call — the listener reads the whole request and answers — is not
    /// cut by any of them however small the call's own deadline is against them.
    #[test]
    fn a_healthy_call_is_not_cut_by_the_ceilings() {
        let port = answering_endpoint("{\"ok\":true}");
        let url = format!("http://127.0.0.1:{port}/v1/chat/completions");
        let started = Instant::now();
        let response = post_json(
            &url,
            &"x".repeat(1024 * 1024),
            None,
            &AtomicBool::new(false),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "{\"ok\":true}");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "a real response is not held up by the phase bounds: {elapsed:?}"
        );
    }

    /// Talks to the configured endpoint; run with `--ignored`.
    #[test]
    #[ignore]
    fn live_models_endpoint() {
        let cfg = mush_core::Config::from_env();
        let response =
            get_json(&cfg.models_url(), cfg.api_key.as_deref(), LIST_READ_TIMEOUT).unwrap();
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
        let response = post_json(
            &cfg.chat_url(),
            &body,
            cfg.api_key.as_deref(),
            &cancel,
            CHAT_DEADLINE,
        )
        .unwrap();
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
            get_json(&cfg.models_url(), cfg.api_key.as_deref(), LIST_READ_TIMEOUT).unwrap();
        assert_eq!(response.status, 401);
    }
}
