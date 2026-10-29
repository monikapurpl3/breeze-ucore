//! HTTP/1.1, as much as an API server needs: a bounded request parser and
//! responses with Breeze Core's headers. One request per connection; the
//! answer says `Connection: close`.

use alloc::string::String;
use alloc::vec::Vec;
use serde::Serialize;

/// Longest request head accepted, and longest body.
pub const MAX_HEAD: usize = 8 * 1024;
pub const MAX_BODY: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    /// As sent, query included: a v2 signature covers it.
    pub target: String,
    /// `target` without the query.
    pub path: String,
    /// Lower-cased names, in the order sent.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    /// The first header of that (lower-case) name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// Not all of it has arrived.
    Incomplete,
    Done(Request),
    /// Unusable; answer with this status and close.
    Bad(u16, &'static str),
}

pub fn parse(buf: &[u8]) -> Parsed {
    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return if buf.len() > MAX_HEAD { Parsed::Bad(431, "request head too large") } else { Parsed::Incomplete };
    };
    if end > MAX_HEAD {
        return Parsed::Bad(431, "request head too large");
    }
    let Ok(head) = core::str::from_utf8(&buf[..end]) else {
        return Parsed::Bad(400, "malformed request");
    };
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (Some(method), Some(target), Some(version)) = (first.next(), first.next(), first.next()) else {
        return Parsed::Bad(400, "malformed request");
    };
    if !version.starts_with("HTTP/1.") || method.is_empty() || !target.starts_with('/') {
        return Parsed::Bad(400, "malformed request");
    }
    let mut headers = Vec::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Parsed::Bad(400, "malformed header");
        };
        headers.push((name.trim().to_ascii_lowercase(), String::from(value.trim())));
    }
    let header = |n: &str| headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str());
    if header("transfer-encoding").is_some_and(|v| !v.eq_ignore_ascii_case("identity")) {
        return Parsed::Bad(411, "send the body with a Content-Length");
    }
    let length = match header("content-length").map(str::parse::<usize>) {
        None => 0,
        Some(Ok(n)) if n <= MAX_BODY => n,
        Some(Ok(_)) => return Parsed::Bad(413, "request body too large"),
        Some(Err(_)) => return Parsed::Bad(400, "malformed Content-Length"),
    };
    let start = end + 4;
    if buf.len() < start + length {
        return Parsed::Incomplete;
    }
    let path = target.split_once('?').map_or(target, |(p, _)| p);
    Parsed::Done(Request {
        method: String::from(method),
        target: String::from(target),
        path: String::from(path),
        headers,
        body: buf[start..start + length].to_vec(),
    })
}

/// Headers on every answer, Breeze Core's character for character.
pub const SECURITY_HEADERS: &[(&str, &str)] = &[
    ("X-Content-Type-Options", "nosniff"),
    ("X-Frame-Options", "DENY"),
    ("Referrer-Policy", "no-referrer"),
    (
        "Content-Security-Policy",
        "default-src 'self'; base-uri 'none'; frame-ancestors 'none'; object-src 'none'; form-action 'self'",
    ),
    ("Strict-Transport-Security", "max-age=63072000; includeSubDomains"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
    pub extra: Vec<(&'static str, String)>,
}

#[derive(Serialize)]
struct DetailBody<'a> {
    detail: &'a str,
}

impl Response {
    pub fn json<T: Serialize + ?Sized>(status: u16, value: &T) -> Self {
        Self { status, body: serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec()), extra: Vec::new() }
    }

    /// `{"detail": "..."}`, the shape clients unwrap for a message.
    pub fn detail(status: u16, message: &str) -> Self {
        Self::json(status, &DetailBody { detail: message })
    }

    pub fn with_header(mut self, name: &'static str, value: &str) -> Self {
        self.extra.push((name, String::from(value)));
        self
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut head = alloc::format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nCache-Control: no-store\r\n",
            self.status,
            reason(self.status)
        );
        for (n, v) in SECURITY_HEADERS {
            head.push_str(n);
            head.push_str(": ");
            head.push_str(v);
            head.push_str("\r\n");
        }
        for (n, v) in &self.extra {
            head.push_str(n);
            head.push_str(": ");
            head.push_str(v);
            head.push_str("\r\n");
        }
        head.push_str(&alloc::format!("Content-Length: {}\r\nConnection: close\r\n\r\n", self.body.len()));
        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        426 => "Upgrade Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_arrives_in_pieces_and_then_parses() {
        let raw = b"POST /api/units/1/control?x=1 HTTP/1.1\r\nHost: a\r\nContent-Length: 4\r\nX-API-Key: k\r\n\r\nbody";
        for n in 0..raw.len() {
            assert_eq!(parse(&raw[..n]), Parsed::Incomplete, "at {n}");
        }
        let Parsed::Done(r) = parse(raw) else { panic!() };
        assert_eq!((r.method.as_str(), r.path.as_str(), r.target.as_str()), ("POST", "/api/units/1/control", "/api/units/1/control?x=1"));
        assert_eq!((r.header("x-api-key"), r.body.as_slice()), (Some("k"), &b"body"[..]));
    }

    #[test]
    fn limits_are_answers_not_panics() {
        assert_eq!(parse(&[b'a'; MAX_HEAD + 1]), Parsed::Bad(431, "request head too large"));
        assert!(matches!(parse(b"GET / HTTP/1.1\r\nContent-Length: 99999999\r\n\r\n"), Parsed::Bad(413, _)));
        assert!(matches!(parse(b"GET /x\r\n\r\n"), Parsed::Bad(400, _)));
    }
}
