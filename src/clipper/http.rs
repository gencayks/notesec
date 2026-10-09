//! Just enough HTTP/1.1 for the clipper: one request per connection, read
//! with hard limits, and a response. Plus the two body formats
//! (`application/x-www-form-urlencoded` and `application/json`, a flat
//! object of strings).
//!
//! Refused: request heads over `MAX_HEAD_BYTES` or `MAX_HEADERS` (431),
//! bodies over the cap (413), `Transfer-Encoding` of any kind (501: our
//! clients always send `Content-Length`, and chunked parsing is attack
//! surface we don't need), a POST without `Content-Length` (411), HTTP
//! versions other than 1.0 / 1.1 (505), folded or malformed header lines
//! (400), and anything slower than the deadline (408).

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

pub const MAX_HEAD_BYTES: usize = 16 * 1024;
pub const MAX_HEADERS: usize = 64;

/// A parsed request.
#[derive(Debug, PartialEq)]
pub struct Request {
    pub method: String,
    /// The path, without the query string.
    pub path: String,
    /// Lowercase names; a name sent twice keeps the first value (`Host`
    /// and `Content-Length` twice are refused instead).
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

/// Why a request was refused: status and a short reason.
#[derive(Debug, PartialEq)]
pub struct HttpError(pub u16, pub &'static str);

/// What to do once the head is read: check it (method, path, Host,
/// token header…) before any body is read. `Err` answers right away.
pub type HeadCheck<'a> = &'a mut dyn FnMut(&Request) -> Result<(), HttpError>;

/// A connection: something to read the request from and answer on.
pub trait Conn: Read + Write {
    /// Bound each blocking read (the deadline shrinks as time passes).
    fn set_timeout(&mut self, timeout: Duration) -> io::Result<()>;
}

impl Conn for std::net::TcpStream {
    fn set_timeout(&mut self, timeout: Duration) -> io::Result<()> {
        self.set_read_timeout(Some(timeout))
    }
}

/// Read one request from `conn`: the head (checked by `check` before
/// anything else is read), then a body of at most `max_body` bytes, all
/// before `deadline`.
pub fn read_request(
    conn: &mut dyn Conn,
    max_body: usize,
    deadline: Instant,
    check: HeadCheck,
) -> Result<Request, HttpError> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(end) = find_head_end(&buf) {
            break end;
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Err(HttpError(431, "request head too large"));
        }
        let n = read_some(conn, &mut chunk, deadline)?;
        if n == 0 {
            return Err(HttpError(400, "connection closed early"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    if head_end.0 > MAX_HEAD_BYTES {
        return Err(HttpError(431, "request head too large"));
    }
    let mut request = parse_head(&buf[..head_end.0])?;
    check(&request)?;

    if request.header("transfer-encoding").is_some() {
        return Err(HttpError(
            501,
            "Transfer-Encoding is not supported; send Content-Length",
        ));
    }
    let length = match request.header("content-length") {
        Some(v) => v
            .parse::<usize>()
            .ok()
            .filter(|_| v.bytes().all(|b| b.is_ascii_digit()))
            .ok_or(HttpError(400, "bad Content-Length"))?,
        None if request.method == "POST" => return Err(HttpError(411, "Content-Length required")),
        None => 0,
    };
    if length > max_body {
        return Err(HttpError(413, "body too large"));
    }
    let mut body = buf[head_end.1..].to_vec();
    if body.len() < length
        && request
            .header("expect")
            .is_some_and(|e| e.eq_ignore_ascii_case("100-continue"))
    {
        conn.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
            .map_err(|_| HttpError(400, "connection closed early"))?;
    }
    while body.len() < length {
        let n = read_some(conn, &mut chunk, deadline)?;
        if n == 0 {
            return Err(HttpError(400, "body shorter than Content-Length"));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    request.body = body;
    Ok(request)
}

fn read_some(conn: &mut dyn Conn, chunk: &mut [u8], deadline: Instant) -> Result<usize, HttpError> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(HttpError(408, "request too slow"));
    }
    conn.set_timeout(left.min(Duration::from_secs(5)))
        .map_err(|_| HttpError(400, "connection error"))?;
    match conn.read(chunk) {
        Ok(n) => Ok(n),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            Err(HttpError(408, "request too slow"))
        }
        Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(read_some(conn, chunk, deadline)?),
        Err(_) => Err(HttpError(400, "connection error")),
    }
}

/// Where the head ends (`\r\n\r\n`, or `\n\n`): (head length, body start).
fn find_head_end(buf: &[u8]) -> Option<(usize, usize)> {
    let crlf = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| (p, p + 4));
    let lf = buf
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|p| (p, p + 2));
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn parse_head(head: &[u8]) -> Result<Request, HttpError> {
    let head =
        std::str::from_utf8(head).map_err(|_| HttpError(400, "request head is not UTF-8"))?;
    let mut lines = head.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
    let line = lines.next().unwrap_or("");
    let parts: Vec<&str> = line.split(' ').collect();
    let [method, target, version] = parts[..] else {
        return Err(HttpError(400, "bad request line"));
    };
    if !is_token(method) {
        return Err(HttpError(400, "bad method"));
    }
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(if version.starts_with("HTTP/") {
            HttpError(505, "HTTP version not supported")
        } else {
            HttpError(400, "bad request line")
        });
    }
    if !target.starts_with('/') {
        return Err(HttpError(400, "bad request target"));
    }
    let path = target.split('?').next().unwrap_or("").to_string();
    let mut headers: HashMap<String, String> = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            return Err(HttpError(400, "folded header lines are not supported"));
        }
        let (name, value) = line
            .split_once(':')
            .ok_or(HttpError(400, "bad header line"))?;
        if !is_token(name) {
            return Err(HttpError(400, "bad header name"));
        }
        if headers.len() >= MAX_HEADERS {
            return Err(HttpError(431, "too many headers"));
        }
        let name = name.to_ascii_lowercase();
        let value = value.trim().to_string();
        if let Some(old) = headers.get(&name) {
            // Two different hosts or lengths: ambiguous, so refused.
            if (name == "host" || name == "content-length") && *old != value {
                return Err(HttpError(400, "conflicting duplicate header"));
            }
            continue;
        }
        headers.insert(name, value);
    }
    Ok(Request {
        method: method.to_string(),
        path,
        headers,
        body: Vec::new(),
    })
}

/// A response to write.
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Response {
        Response {
            status,
            headers: vec![("Content-Type", content_type.to_string())],
            body: body.into(),
        }
    }

    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Response {
        self.headers.push((name, value.into()));
        self
    }

    /// The bytes on the wire: always `Connection: close`, a length, no
    /// caching or sniffing, no referrer.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {} {}\r\n", self.status, reason(self.status));
        for (name, value) in &self.headers {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
        out.push_str(&format!(
            "Content-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\r\n",
            self.body.len()
        ));
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
    }
}

pub fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        505 => "HTTP Version Not Supported",
        _ => "Unknown",
    }
}

/// `a=1&b=x+y%21` as fields (`+` is a space; bad escapes stay as written;
/// invalid UTF-8 becomes U+FFFD). A name sent twice keeps the first.
pub fn parse_form(body: &[u8]) -> HashMap<String, String> {
    let mut fields = HashMap::new();
    for pair in body.split(|&b| b == b'&') {
        if pair.is_empty() {
            continue;
        }
        let (name, value) = match pair.iter().position(|&b| b == b'=') {
            Some(at) => (&pair[..at], &pair[at + 1..]),
            None => (pair, &b""[..]),
        };
        fields
            .entry(form_decode(name))
            .or_insert_with(|| form_decode(value));
    }
    fields
}

fn form_decode(bytes: &[u8]) -> String {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A JSON object's string members. Members of other types are skipped
/// (nesting at most 32 deep); anything but an object, invalid JSON or
/// trailing text is an error. A name sent twice keeps the last.
pub fn parse_json(body: &[u8]) -> Result<HashMap<String, String>, &'static str> {
    let text = std::str::from_utf8(body).map_err(|_| "body is not UTF-8")?;
    let mut p = Json {
        s: text.as_bytes(),
        i: 0,
    };
    p.ws();
    let mut fields = HashMap::new();
    p.expect(b'{')?;
    p.ws();
    if p.peek() == Some(b'}') {
        p.i += 1;
    } else {
        loop {
            p.ws();
            let name = p.string()?;
            p.ws();
            p.expect(b':')?;
            p.ws();
            if p.peek() == Some(b'"') {
                let value = p.string()?;
                fields.insert(name, value);
            } else {
                p.skip_value(0)?;
            }
            p.ws();
            match p.next() {
                Some(b',') => continue,
                Some(b'}') => break,
                _ => return Err("expected , or }"),
            }
        }
    }
    p.ws();
    if p.i != p.s.len() {
        return Err("text after the JSON object");
    }
    Ok(fields)
}

struct Json<'a> {
    s: &'a [u8],
    i: usize,
}

impl Json<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let b = self.peek();
        self.i += 1;
        b
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn expect(&mut self, b: u8) -> Result<(), &'static str> {
        if self.next() == Some(b) {
            Ok(())
        } else {
            Err("invalid JSON")
        }
    }

    fn hex4(&mut self) -> Result<u32, &'static str> {
        let digits = self.s.get(self.i..self.i + 4).ok_or("bad \\u escape")?;
        let text = std::str::from_utf8(digits).map_err(|_| "bad \\u escape")?;
        let v = u32::from_str_radix(text, 16).map_err(|_| "bad \\u escape")?;
        self.i += 4;
        Ok(v)
    }

    fn string(&mut self) -> Result<String, &'static str> {
        self.expect(b'"')?;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let b = self.next().ok_or("unterminated string")?;
            match b {
                b'"' => break,
                b'\\' => {
                    let e = self.next().ok_or("unterminated string")?;
                    let c = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let hi = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&hi)
                                && self.s.get(self.i..self.i + 2) == Some(b"\\u")
                            {
                                let save = self.i;
                                self.i += 2;
                                let lo = self.hex4()?;
                                if (0xDC00..0xE000).contains(&lo) {
                                    0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                                } else {
                                    self.i = save;
                                    0xFFFD
                                }
                            } else {
                                hi
                            };
                            char::from_u32(code).unwrap_or('\u{fffd}')
                        }
                        _ => return Err("bad escape"),
                    };
                    let mut tmp = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                }
                0..=0x1f => return Err("control character in string"),
                b => out.push(b),
            }
        }
        // The input was UTF-8 and escapes add whole characters.
        String::from_utf8(out).map_err(|_| "invalid string")
    }

    fn skip_value(&mut self, depth: usize) -> Result<(), &'static str> {
        if depth > 32 {
            return Err("JSON nested too deep");
        }
        self.ws();
        match self.peek().ok_or("invalid JSON")? {
            b'"' => self.string().map(|_| ()),
            b'{' | b'[' => {
                let close = if self.next() == Some(b'{') {
                    b'}'
                } else {
                    b']'
                };
                self.ws();
                if self.peek() == Some(close) {
                    self.i += 1;
                    return Ok(());
                }
                loop {
                    self.ws();
                    if close == b'}' {
                        self.string()?;
                        self.ws();
                        self.expect(b':')?;
                    }
                    self.skip_value(depth + 1)?;
                    self.ws();
                    match self.next() {
                        Some(b',') => continue,
                        Some(b) if b == close => return Ok(()),
                        _ => return Err("invalid JSON"),
                    }
                }
            }
            _ => {
                let start = self.i;
                while matches!(self.peek(), Some(b) if b.is_ascii_alphanumeric() || b"+-.".contains(&b))
                {
                    self.i += 1;
                }
                let word = &self.s[start..self.i];
                let number = word
                    .first()
                    .is_some_and(|b| b.is_ascii_digit() || *b == b'-');
                if word == b"true"
                    || word == b"false"
                    || word == b"null"
                    || (number && word.len() < 40)
                {
                    Ok(())
                } else {
                    Err("invalid JSON")
                }
            }
        }
    }
}

/// `text` for a JSON string literal (quotes included).
pub fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // `<` too, so the JSON is inert even if something renders it.
            c if (c as u32) < 0x20 || c == '<' || c == '>' || c == '&' => {
                out.push_str(&format!("\\u{:04x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `text` safe inside HTML text or a quoted attribute.
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::io::Cursor;

    /// A connection over bytes: what the client sends (in segments that
    /// never arrive in one read, like separate packets), what we answer.
    pub struct Fake {
        pub input: std::collections::VecDeque<Cursor<Vec<u8>>>,
        pub output: Vec<u8>,
    }

    impl Fake {
        pub fn new(input: impl Into<Vec<u8>>) -> Fake {
            Fake::segments(vec![input.into()])
        }

        pub fn segments(parts: Vec<Vec<u8>>) -> Fake {
            Fake {
                input: parts.into_iter().map(Cursor::new).collect(),
                output: Vec::new(),
            }
        }
    }

    impl Read for Fake {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            // A trickle: 7 bytes at a time, so every boundary is crossed.
            let n = buf.len().min(7);
            while let Some(front) = self.input.front_mut() {
                let got = front.read(&mut buf[..n])?;
                if got > 0 {
                    return Ok(got);
                }
                self.input.pop_front();
            }
            Ok(0)
        }
    }

    impl Write for Fake {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Conn for Fake {
        fn set_timeout(&mut self, _: Duration) -> io::Result<()> {
            Ok(())
        }
    }

    fn read(raw: &str, max_body: usize) -> Result<Request, HttpError> {
        let mut conn = Fake::new(raw);
        let deadline = Instant::now() + Duration::from_secs(5);
        read_request(&mut conn, max_body, deadline, &mut |_| Ok(()))
    }

    #[test]
    fn requests_parse_with_limits() {
        let r = read(
            "POST /clip?x=1 HTTP/1.1\r\nHost: 127.0.0.1:9\r\nContent-Type: application/json\r\nContent-Length: 5\r\n\r\nhelloEXTRA",
            100,
        )
        .unwrap();
        assert_eq!((r.method.as_str(), r.path.as_str()), ("POST", "/clip"));
        assert_eq!(r.header("host"), Some("127.0.0.1:9"));
        assert_eq!(r.body, b"hello");
        // Bare LF line ends are read too.
        assert!(read("OPTIONS /clip HTTP/1.1\nHost: a\n\n", 10).is_ok());

        let err = |raw: &str| read(raw, 10).unwrap_err().0;
        assert_eq!(err("POST /clip HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n"), 501);
        assert_eq!(err("POST /clip HTTP/1.1\r\nHost: a\r\n\r\n"), 411);
        assert_eq!(
            err("POST /clip HTTP/1.1\r\nHost: a\r\nContent-Length: 11\r\n\r\n"),
            413
        );
        assert_eq!(
            err("POST /clip HTTP/1.1\r\nHost: a\r\nContent-Length: -1\r\n\r\n"),
            400
        );
        assert_eq!(
            err("POST /clip HTTP/1.1\r\nHost: a\r\nContent-Length: +5\r\n\r\n"),
            400
        );
        assert_eq!(
            err("POST /clip HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\n\r\nab"),
            400
        );
        assert_eq!(
            err("POST /clip HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n"),
            400
        );
        assert_eq!(
            err("POST /clip HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n"),
            400
        );
        assert_eq!(err("POST /clip HTTP/2.0\r\n\r\n"), 505);
        assert_eq!(err("POST /clip\r\n\r\n"), 400);
        assert_eq!(err("POST http://x/clip HTTP/1.1\r\n\r\n"), 400);
        assert_eq!(err("POST /clip HTTP/1.1\r\nX: a\r\n folded\r\n\r\n"), 400);
        assert_eq!(err("POST /clip HTTP/1.1\r\nBad Name: a\r\n\r\n"), 400);
        assert_eq!(err("POST /clip HTTP/1.1\r\nHost: a"), 400);
        let many: String = (0..MAX_HEADERS + 1)
            .map(|i| format!("X-{i}: v\r\n"))
            .collect();
        assert_eq!(err(&format!("GET /clip HTTP/1.1\r\n{many}\r\n")), 431);
        let long = "a".repeat(MAX_HEAD_BYTES + 10);
        assert_eq!(
            err(&format!("GET /clip HTTP/1.1\r\nX: {long}\r\n\r\n")),
            431
        );
        assert_eq!(err(&format!("GET /clip HTTP/1.1\r\nX: {long}")), 431);

        // The head check runs before any body is read.
        let mut conn = Fake::new("POST /clip HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello");
        let deadline = Instant::now() + Duration::from_secs(5);
        let refused = read_request(&mut conn, 100, deadline, &mut |_| Err(HttpError(403, "no")));
        assert_eq!(refused, Err(HttpError(403, "no")));
        // A deadline already past: 408.
        let mut conn = Fake::new("POST /clip HTTP/1.1\r\n");
        let past = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            read_request(&mut conn, 100, past, &mut |_| Ok(()))
                .unwrap_err()
                .0,
            408
        );
        // Expect: 100-continue gets its interim answer.
        let mut conn = Fake::segments(vec![
            b"POST /clip HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: 2\r\n\r\n".to_vec(),
            b"hi".to_vec(),
        ]);
        let r = read_request(&mut conn, 100, deadline, &mut |_| Ok(())).unwrap();
        assert_eq!(r.body, b"hi");
        assert_eq!(conn.output, b"HTTP/1.1 100 Continue\r\n\r\n");
    }

    #[test]
    fn form_and_json_bodies() {
        let form = parse_form(b"token=abc&title=A+%26+B&html=%3Cp%3Ex%3C%2Fp%3E&bad=%zz%4&title=second&empty&u=%C3%A9%FF");
        assert_eq!(form["token"], "abc");
        assert_eq!(form["title"], "A & B");
        assert_eq!(form["html"], "<p>x</p>");
        assert_eq!(form["bad"], "%zz%4");
        assert_eq!(form["empty"], "");
        assert_eq!(form["u"], "é\u{fffd}");

        let json = parse_json(
            br#" {"token":"t","title":"A \"q\" \u00e9 \ud83d\ude00 \ud800x","n":-1.5e3,"b":true,"x":null,"o":{"a":[1,{"b":[]}]},"html":"<p>\n</p>"} "#,
        )
        .unwrap();
        assert_eq!(json["token"], "t");
        assert_eq!(json["title"], "A \"q\" é 😀 \u{fffd}x");
        assert_eq!(json["html"], "<p>\n</p>");
        assert!(!json.contains_key("n"));
        for bad in [
            &b"[]"[..],
            b"{",
            b"{\"a\":}",
            b"{\"a\":1}x",
            b"{\"a\":\"\x01\"}",
            b"{\"a\":tru}",
            b"\xff",
            b"{\"a\":\"\\q\"}",
            b"{a:1}",
        ] {
            assert!(parse_json(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        let deep = format!("{{\"a\":{}{}}}", "[".repeat(40), "]".repeat(40));
        assert!(parse_json(deep.as_bytes()).is_err());
        assert_eq!(parse_json(b"{}").unwrap().len(), 0);

        assert_eq!(
            json_string("a\"<b>\n\u{1}"),
            r#""a\"\u003cb\u003e\n\u0001""#
        );
        assert_eq!(
            escape_html("<a href=\"x\">'&'</a>"),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;"
        );
    }
}
