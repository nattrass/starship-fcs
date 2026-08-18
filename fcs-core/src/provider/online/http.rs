//! Enough HTTP/1.1 to post a JSON body and read a JSON body back, hand-rolled
//! so the `online` feature stays dependency-free.
//!
//! The split matters: this module owns the *framing* — how a request becomes
//! bytes and how bytes become a response — while an
//! [`HttpTransport`](super::HttpTransport) owns the *connection*. That is
//! what makes TLS pluggable without this file knowing anything about it, and
//! what lets a test drive an adapter end to end without a socket.
//!
//! Everything parsed here arrives from the network and is treated as such:
//! sizes are bounded, a malformed frame is an error rather than a
//! best-effort read, and nothing is retried. A ship that cannot get a
//! sentence out of a model flies the tick on its autopilot instead.

use std::fmt;

/// The largest response body that will be read. A model's turn is a few
/// hundred bytes of protocol; anything approaching this is a malfunction or
/// an attack, and either way the tick is better spent on the autopilot.
pub const MAX_BODY_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    pub fn default_port(self) -> u16 {
        match self {
            Scheme::Http => 80,
            Scheme::Https => 443,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub scheme: Scheme,
    pub host: String,
    pub port: u16,
    /// Path and query, already joined — what goes on the request line.
    pub path: String,
}

impl Url {
    /// The `host` header value: the bare host, or `host:port` when the port
    /// is not the scheme's default.
    pub fn authority(&self) -> String {
        if self.port == self.scheme.default_port() {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// Parses `scheme://host[:port][/path]`. Deliberately narrow — no userinfo,
/// no fragments, no relative forms. An endpoint is configuration, not user
/// input, and a shape this module does not understand is an error rather
/// than something to interpret generously.
pub fn parse_url(raw: &str) -> Result<Url, HttpError> {
    let (scheme, rest) = match raw.split_once("://") {
        Some(("http", rest)) => (Scheme::Http, rest),
        Some(("https", rest)) => (Scheme::Https, rest),
        Some((other, _)) => return Err(HttpError::UnsupportedScheme(other.to_string())),
        None => return Err(HttpError::MalformedUrl(raw.to_string())),
    };

    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(HttpError::MalformedUrl(raw.to_string()));
    }

    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => {
            let port: u16 = port
                .parse()
                .map_err(|_| HttpError::MalformedUrl(raw.to_string()))?;
            (host, port)
        }
        None => (authority, scheme.default_port()),
    };
    if host.is_empty() {
        return Err(HttpError::MalformedUrl(raw.to_string()));
    }

    Ok(Url {
        scheme,
        host: host.to_string(),
        port,
        path: path.to_string(),
    })
}

/// One request to make. Always a POST of a JSON body — that is the only shape
/// a chat completions endpoint needs, and the only one this module builds.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpRequest {
    pub url: Url,
    /// Extra headers, lowercase by convention. `host`, `content-length`,
    /// `content-type`, `accept`, and `connection` are supplied by
    /// [`HttpRequest::to_wire`] and must not be repeated here.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl HttpRequest {
    pub fn post_json(url: Url, headers: Vec<(String, String)>, body: String) -> Self {
        Self { url, headers, body }
    }

    /// Renders the request as bytes. `connection: close` is deliberate: one
    /// turn is one connection, so there is no pool to keep consistent and no
    /// half-used socket to carry between ticks.
    pub fn to_wire(&self) -> Vec<u8> {
        let mut head = format!("POST {} HTTP/1.1\r\n", self.url.path);
        head.push_str(&format!("host: {}\r\n", self.url.authority()));
        head.push_str("content-type: application/json\r\n");
        head.push_str("accept: application/json\r\n");
        head.push_str("connection: close\r\n");
        head.push_str(&format!("content-length: {}\r\n", self.body.len()));
        for (name, value) in &self.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");

        let mut wire = head.into_bytes();
        wire.extend_from_slice(self.body.as_bytes());
        wire
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

impl HttpResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum HttpError {
    MalformedUrl(String),
    UnsupportedScheme(String),
    MalformedStatusLine,
    MalformedHeaders,
    MalformedChunk,
    /// The response ended before the body it promised.
    Truncated,
    BodyTooLarge,
    NotUtf8,
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpError::MalformedUrl(raw) => write!(f, "malformed url: {raw}"),
            HttpError::UnsupportedScheme(scheme) => write!(f, "unsupported url scheme: {scheme}"),
            HttpError::MalformedStatusLine => write!(f, "malformed HTTP status line"),
            HttpError::MalformedHeaders => write!(f, "malformed HTTP headers"),
            HttpError::MalformedChunk => write!(f, "malformed chunked body"),
            HttpError::Truncated => write!(f, "response ended mid-body"),
            HttpError::BodyTooLarge => write!(f, "response body exceeds {MAX_BODY_BYTES} bytes"),
            HttpError::NotUtf8 => write!(f, "response body is not valid UTF-8"),
        }
    }
}

impl std::error::Error for HttpError {}

/// Parses a whole response: status line, headers, and a body framed by either
/// `content-length` or `transfer-encoding: chunked`.
pub fn parse_response(bytes: &[u8]) -> Result<HttpResponse, HttpError> {
    let split = find(bytes, b"\r\n\r\n").ok_or(HttpError::Truncated)?;
    let head = std::str::from_utf8(&bytes[..split]).map_err(|_| HttpError::MalformedHeaders)?;
    let rest = &bytes[split + 4..];

    let mut lines = head.split("\r\n");
    let status = parse_status_line(lines.next().ok_or(HttpError::MalformedStatusLine)?)?;

    let mut content_length = None;
    let mut chunked = false;
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(HttpError::MalformedHeaders)?;
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "content-length" => {
                content_length = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| HttpError::MalformedHeaders)?,
                );
            }
            "transfer-encoding" => {
                chunked = value.to_ascii_lowercase().contains("chunked");
            }
            _ => {}
        }
    }

    // Chunked wins if both are present: a proxy that sent both is describing
    // the framing it actually used with the transfer encoding.
    let body = if chunked {
        dechunk(rest)?
    } else if let Some(length) = content_length {
        if length > MAX_BODY_BYTES {
            return Err(HttpError::BodyTooLarge);
        }
        if rest.len() < length {
            return Err(HttpError::Truncated);
        }
        rest[..length].to_vec()
    } else {
        // No framing header: the body runs to the close of the connection.
        if rest.len() > MAX_BODY_BYTES {
            return Err(HttpError::BodyTooLarge);
        }
        rest.to_vec()
    };

    Ok(HttpResponse {
        status,
        body: String::from_utf8(body).map_err(|_| HttpError::NotUtf8)?,
    })
}

fn parse_status_line(line: &str) -> Result<u16, HttpError> {
    let mut parts = line.split(' ');
    let version = parts.next().ok_or(HttpError::MalformedStatusLine)?;
    if !version.starts_with("HTTP/") {
        return Err(HttpError::MalformedStatusLine);
    }
    parts
        .next()
        .ok_or(HttpError::MalformedStatusLine)?
        .parse()
        .map_err(|_| HttpError::MalformedStatusLine)
}

/// Reassembles a `transfer-encoding: chunked` body. Trailers after the
/// terminating zero-length chunk are ignored, which is what they are for.
fn dechunk(mut rest: &[u8]) -> Result<Vec<u8>, HttpError> {
    let mut body = Vec::new();

    loop {
        let line_end = find(rest, b"\r\n").ok_or(HttpError::Truncated)?;
        let header =
            std::str::from_utf8(&rest[..line_end]).map_err(|_| HttpError::MalformedChunk)?;
        // A chunk header may carry `;extensions` after the size.
        let size_token = header.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_token, 16).map_err(|_| HttpError::MalformedChunk)?;

        rest = &rest[line_end + 2..];
        if size == 0 {
            return Ok(body);
        }
        if body.len() + size > MAX_BODY_BYTES {
            return Err(HttpError::BodyTooLarge);
        }
        if rest.len() < size + 2 {
            return Err(HttpError::Truncated);
        }

        body.extend_from_slice(&rest[..size]);
        if &rest[size..size + 2] != b"\r\n" {
            return Err(HttpError::MalformedChunk);
        }
        rest = &rest[size + 2..];
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_url_forms_an_endpoint_is_configured_with() {
        assert_eq!(
            parse_url("https://api.anthropic.com/v1/messages").unwrap(),
            Url {
                scheme: Scheme::Https,
                host: "api.anthropic.com".into(),
                port: 443,
                path: "/v1/messages".into(),
            }
        );
        assert_eq!(
            parse_url("http://localhost:11434/api/chat").unwrap(),
            Url {
                scheme: Scheme::Http,
                host: "localhost".into(),
                port: 11434,
                path: "/api/chat".into(),
            }
        );
        // No path is the root path.
        assert_eq!(parse_url("http://example.com").unwrap().path, "/");
    }

    #[test]
    fn the_host_header_carries_a_port_only_when_it_is_not_the_default() {
        assert_eq!(
            parse_url("https://api.anthropic.com/v1/messages")
                .unwrap()
                .authority(),
            "api.anthropic.com"
        );
        assert_eq!(
            parse_url("http://localhost:11434/api/chat")
                .unwrap()
                .authority(),
            "localhost:11434"
        );
    }

    #[test]
    fn a_url_shape_this_module_does_not_understand_is_refused() {
        assert!(matches!(
            parse_url("ftp://example.com/x"),
            Err(HttpError::UnsupportedScheme(_))
        ));
        assert!(matches!(
            parse_url("api.anthropic.com/v1/messages"),
            Err(HttpError::MalformedUrl(_))
        ));
        assert!(matches!(
            parse_url("https:///v1/messages"),
            Err(HttpError::MalformedUrl(_))
        ));
        assert!(matches!(
            parse_url("http://example.com:not-a-port/x"),
            Err(HttpError::MalformedUrl(_))
        ));
    }

    fn request() -> HttpRequest {
        HttpRequest::post_json(
            parse_url("https://api.anthropic.com/v1/messages").unwrap(),
            vec![("x-api-key".into(), "secret".into())],
            r#"{"a":1}"#.to_string(),
        )
    }

    #[test]
    fn renders_a_post_with_the_framing_headers_it_owns() {
        let wire = String::from_utf8(request().to_wire()).unwrap();

        assert!(wire.starts_with("POST /v1/messages HTTP/1.1\r\n"));
        assert!(wire.contains("host: api.anthropic.com\r\n"));
        assert!(wire.contains("content-type: application/json\r\n"));
        assert!(wire.contains("content-length: 7\r\n"));
        assert!(wire.contains("connection: close\r\n"));
        assert!(wire.contains("x-api-key: secret\r\n"));
        assert!(wire.ends_with("\r\n\r\n{\"a\":1}"));
    }

    /// `content-length` counts bytes, not characters — a prompt with any
    /// non-ASCII in it would otherwise be silently truncated by the server.
    #[test]
    fn content_length_counts_bytes_rather_than_characters() {
        let body = r#"{"t":"🚀"}"#.to_string();
        let byte_length = body.len();
        let request = HttpRequest::post_json(
            parse_url("http://localhost:11434/api/chat").unwrap(),
            Vec::new(),
            body,
        );

        let wire = String::from_utf8(request.to_wire()).unwrap();
        assert!(wire.contains(&format!("content-length: {byte_length}\r\n")));
        // Nine characters, twelve bytes — the rocket is four of them.
        assert_eq!(byte_length, 12);
    }

    #[test]
    fn parses_a_content_length_framed_response() {
        let raw = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 9\r\n\r\n{\"ok\":1}\n";
        let response = parse_response(raw).unwrap();

        assert_eq!(response.status, 200);
        assert_eq!(response.body, "{\"ok\":1}\n");
        assert!(response.is_success());
    }

    #[test]
    fn parses_a_chunked_response() {
        let raw = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\n{\"ok\"\r\n3\r\n:1}\r\n0\r\n\r\n";
        let response = parse_response(raw).unwrap();

        assert_eq!(response.status, 200);
        assert_eq!(response.body, "{\"ok\":1}");
    }

    #[test]
    fn chunk_extensions_and_trailers_do_not_confuse_the_reader() {
        let raw = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n2;foo=bar\r\nhi\r\n0\r\nx-trailer: y\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap().body, "hi");
    }

    /// Some servers close the connection instead of framing the body.
    #[test]
    fn a_body_with_no_framing_header_runs_to_the_close() {
        let raw = b"HTTP/1.1 200 OK\r\n\r\n{\"ok\":1}";
        assert_eq!(parse_response(raw).unwrap().body, "{\"ok\":1}");
    }

    #[test]
    fn a_non_success_status_is_reported_rather_than_hidden() {
        let raw = b"HTTP/1.1 429 Too Many Requests\r\ncontent-length: 2\r\n\r\n{}";
        let response = parse_response(raw).unwrap();

        assert_eq!(response.status, 429);
        assert!(!response.is_success());
        assert_eq!(response.body, "{}");
    }

    #[test]
    fn header_names_are_matched_case_insensitively() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi";
        assert_eq!(parse_response(raw).unwrap().body, "hi");
    }

    #[test]
    fn a_truncated_response_is_an_error_rather_than_a_short_read() {
        let raw = b"HTTP/1.1 200 OK\r\ncontent-length: 40\r\n\r\ntoo short";
        assert_eq!(parse_response(raw), Err(HttpError::Truncated));

        let no_headers = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n";
        assert_eq!(parse_response(no_headers), Err(HttpError::Truncated));
    }

    #[test]
    fn a_malformed_frame_is_refused_rather_than_guessed_at() {
        assert_eq!(
            parse_response(b"nonsense\r\n\r\n"),
            Err(HttpError::MalformedStatusLine)
        );
        assert_eq!(
            parse_response(b"HTTP/1.1 200 OK\r\nnot-a-header\r\n\r\n"),
            Err(HttpError::MalformedHeaders)
        );
        assert_eq!(
            parse_response(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\nzz\r\n"),
            Err(HttpError::MalformedChunk)
        );
    }

    /// A hostile or malfunctioning peer must not be able to make the ship
    /// read an unbounded body.
    #[test]
    fn a_body_past_the_size_cap_is_refused() {
        let raw = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        assert_eq!(parse_response(raw.as_bytes()), Err(HttpError::BodyTooLarge));
    }

    #[test]
    fn a_body_that_is_not_utf8_is_refused() {
        let mut raw = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n".to_vec();
        raw.extend_from_slice(&[0xff, 0xfe]);
        assert_eq!(parse_response(&raw), Err(HttpError::NotUtf8));
    }
}
