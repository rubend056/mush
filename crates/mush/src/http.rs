//! A tiny blocking HTTP/1.1 client.
//!
//! mush only ever talks to OpenAI-compatible endpoints, so a whole HTTP stack
//! would be overkill. This handles exactly what we need: one request per
//! connection, with `Content-Length` or chunked responses, over plain HTTP or
//! TLS (rustls). Keeping it in-tree means no framework and no runtime to debug.
//!
//! A chat request can be *watched*: the socket is read in short slices and the
//! caller's cancellation flag is polled between them, so Ctrl-C interrupts a
//! model that is still thinking instead of waiting for its reply.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mush_core::Config;

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

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

/// Anything the client can read from and write to: a plain TCP stream or a
/// rustls TLS stream. `Box<dyn Read + Write>` is not a valid trait object, so
/// one supertrait is needed.
trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

pub fn get_json(url: &str, api_key: Option<&str>, read_timeout: Duration) -> io::Result<Response> {
    request("GET", url, None, api_key, read_timeout, None)
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
        "POST",
        url,
        Some(body),
        api_key,
        CHAT_READ_TIMEOUT,
        Some(cancel),
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

fn request(
    method: &str,
    url: &str,
    body: Option<&str>,
    api_key: Option<&str>,
    read_timeout: Duration,
    cancel: Option<&AtomicBool>,
) -> io::Result<Response> {
    let watch = Watch::new(cancel, read_timeout);
    // A Stop that arrived before the request did: do not pay for a call the
    // human already cancelled.
    watch.check()?;

    let (host, port, path, tls) = parse_url(url)?;
    let mut stream = connect(&host, port, tls, read_timeout)?;

    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\nAccept: application/json\r\n"
    );
    if let Some(key) = api_key {
        head.push_str(&format!("Authorization: Bearer {key}\r\n"));
    }
    if let Some(body) = body {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    head.push_str("\r\n");

    stream.write_all(head.as_bytes())?;
    if let Some(body) = body {
        stream.write_all(body.as_bytes())?;
    }
    stream.flush()?;

    let mut reader = BufReader::new(stream);

    let status_line = read_line(&mut reader, &watch)?.unwrap_or_default();
    let status = parse_status(&status_line)?;

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    while let Some(line) = read_line(&mut reader, &watch)? {
        if line.is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse().ok();
        } else if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
            chunked = true;
        }
    }

    let body = if chunked {
        read_chunked(&mut reader, &watch)?
    } else if let Some(len) = content_length {
        String::from_utf8_lossy(&read_exact(&mut reader, len, &watch)?).into_owned()
    } else {
        String::from_utf8_lossy(&read_to_end(&mut reader, &watch)?).into_owned()
    };

    Ok(Response { status, body })
}

/// A cancellation flag and a deadline, threaded through one request's reads.
///
/// The socket is given [`READ_SLICE`] as its read timeout, so every read wakes
/// up quickly; `check` is what turns those wake-ups into a decision — keep
/// waiting, or stop because the human asked us to.
struct Watch<'a> {
    cancel: Option<&'a AtomicBool>,
    deadline: Instant,
}

impl<'a> Watch<'a> {
    fn new(cancel: Option<&'a AtomicBool>, timeout: Duration) -> Self {
        Self {
            cancel,
            deadline: Instant::now() + timeout,
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel
            .map(|flag| flag.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    /// A read timed out: keep waiting unless the human cancelled or the
    /// endpoint has been silent for longer than the whole request may take.
    fn check(&self) -> io::Result<()> {
        if self.cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "request cancelled",
            ));
        }
        if Instant::now() >= self.deadline {
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

/// One bufferful from the socket, retrying the read slices. `Ok(None)` is EOF.
///
/// Everything below reads through `fill_buf`/`consume` rather than
/// `read_line`/`read_exact`: a timeout leaves the buffer untouched, so the
/// retry cannot lose a half-read line — which is exactly what a cancellation
/// arriving mid-body would otherwise do.
fn fill<'b, R: BufRead>(reader: &'b mut R, watch: &Watch) -> io::Result<Option<&'b [u8]>> {
    loop {
        match reader.fill_buf() {
            Ok([]) => return Ok(None),
            Ok(buffer) => return Ok(Some(buffer)),
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

/// Everything up to end of stream.
fn read_to_end<R: BufRead>(reader: &mut R, watch: &Watch) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = fill(reader, watch)? {
        out.extend_from_slice(chunk);
        let take = chunk.len();
        reader.consume(take);
    }
    Ok(out)
}

fn connect(
    host: &str,
    port: u16,
    tls: bool,
    read_timeout: Duration,
) -> io::Result<Box<dyn ReadWrite>> {
    let mut last_error = None;
    for address in (host, port).to_socket_addrs()? {
        let stream = match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(stream) => stream,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        // Liveness guards, not UX timers: a stalled endpoint must not pin a
        // thread (and, for the model list, the whole TUI) forever. During
        // setup the socket gets the whole budget — a TLS handshake is a
        // conversation, not a read — and only then the short slice that lets a
        // cancellation land while the model thinks.
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
        stream.set_read_timeout(Some(read_timeout))?;
        if tls {
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
    stream.flush()?; // completes the handshake
    Ok(stream)
}

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
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host.to_string(),
            port.parse::<u16>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, format!("bad port in {url}"))
            })?,
        ),
        None => (authority.to_string(), if tls { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("no host in {url}"),
        ));
    }
    Ok((host, port, path, tls))
}

fn parse_status(line: &str) -> io::Result<u16> {
    line.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("malformed status line: {line:?}"),
            )
        })
}

fn read_chunked<R: BufRead>(reader: &mut R, watch: &Watch) -> io::Result<String> {
    let mut out = Vec::new();
    loop {
        let size_line = read_line(reader, watch)?.unwrap_or_default();
        let size_field = size_line.trim().split(';').next().unwrap_or("0").trim();
        let size = usize::from_str_radix(size_field, 16).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("malformed chunk size: {size_field:?}"),
            )
        })?;
        if size == 0 {
            break;
        }
        out.extend_from_slice(&read_exact(reader, size, watch)?);
        let _crlf = read_exact(reader, 2, watch)?;
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
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
        };
        let response =
            get_json(&cfg.models_url(), cfg.api_key.as_deref(), CHAT_READ_TIMEOUT).unwrap();
        assert_eq!(response.status, 401);
    }
}
