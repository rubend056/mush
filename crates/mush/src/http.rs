//! A tiny blocking HTTP/1.1 client.
//!
//! mush only ever talks to OpenAI-compatible endpoints, so a whole HTTP stack
//! would be overkill. This handles exactly what we need: one request per
//! connection, with `Content-Length` or chunked responses, over plain HTTP or
//! TLS (rustls). Keeping it in-tree means no framework and no runtime to debug.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use mush_core::Config;

/// Fail fast when the endpoint is unreachable, rather than inheriting the
/// operating system's multi-minute connect timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A chat completion may legitimately take minutes on a slow local model.
const CHAT_READ_TIMEOUT: Duration = Duration::from_secs(600);
/// Listing models must never freeze the caller: the UI thread does this when
/// `/model`, `/url`, or `/key` runs, and a stalled endpoint should just fall
/// back to the provider's known list.
const LIST_READ_TIMEOUT: Duration = Duration::from_secs(10);

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
    request("GET", url, None, api_key, read_timeout)
}

pub fn post_json(url: &str, body: &str, api_key: Option<&str>) -> io::Result<Response> {
    request("POST", url, Some(body), api_key, CHAT_READ_TIMEOUT)
}

/// List model ids advertised by the endpoint. Falls back to the provider's
/// known models when the endpoint is unreachable or lacks `/v1/models`.
pub fn list_models(cfg: &Config) -> Vec<String> {
    let known = cfg.default_models();
    let response = match get_json(&cfg.models_url(), cfg.api_key.as_deref(), LIST_READ_TIMEOUT) {
        Ok(response) if response.status == 200 => response,
        _ => return known,
    };
    let value: serde_json::Value = match serde_json::from_str(&response.body) {
        Ok(value) => value,
        Err(_) => return known,
    };
    let ids: Vec<String> = value
        .get("data")
        .and_then(serde_json::Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|m| m.get("id").and_then(serde_json::Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        known
    } else {
        ids
    }
}

fn request(
    method: &str,
    url: &str,
    body: Option<&str>,
    api_key: Option<&str>,
    read_timeout: Duration,
) -> io::Result<Response> {
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

    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    let status = parse_status(&status_line)?;

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
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
        read_chunked(&mut reader)?
    } else if let Some(len) = content_length {
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf)?;
        String::from_utf8_lossy(&buf).into_owned()
    } else {
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf)?;
        String::from_utf8_lossy(&buf).into_owned()
    };

    Ok(Response { status, body })
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
        // thread (and, for the model list, the whole TUI) forever.
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        stream.set_read_timeout(Some(read_timeout))?;
        return if tls {
            tls_connect(host, stream).map(|stream| Box::new(stream) as Box<dyn ReadWrite>)
        } else {
            Ok(Box::new(stream))
        };
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

fn read_chunked<R: BufRead>(reader: &mut R) -> io::Result<String> {
    let mut out = Vec::new();
    loop {
        let mut size_line = String::new();
        reader.read_line(&mut size_line)?;
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
        let mut chunk = vec![0u8; size];
        reader.read_exact(&mut chunk)?;
        out.extend_from_slice(&chunk);
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf)?;
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

    /// An endpoint that accepts the connection and then says nothing must
    /// time out, not wedge the caller: this is what keeps a stalled
    /// `/v1/models` from freezing the TUI.
    #[test]
    fn a_silent_endpoint_times_out() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Accept exactly one connection and hold it open without answering.
        std::thread::spawn(move || {
            if let Ok(connection) = listener.accept() {
                std::thread::sleep(Duration::from_secs(30));
                drop(connection);
            }
        });

        let url = format!("http://127.0.0.1:{port}/v1/models");
        let started = Instant::now();
        let result = get_json(&url, None, Duration::from_millis(300));
        assert!(
            result.is_err(),
            "a silent endpoint must not look like a success"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "took {:?}",
            started.elapsed()
        );
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
        };
        let response =
            get_json(&cfg.models_url(), cfg.api_key.as_deref(), CHAT_READ_TIMEOUT).unwrap();
        assert_eq!(response.status, 401);
    }
}
