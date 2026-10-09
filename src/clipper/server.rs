//! The listener: 127.0.0.1 only, a few connections at a time, one request
//! each, checked in this order: head limits, Host (DNS rebinding), path,
//! method, body type, a token header if sent; then the body (size cap,
//! deadline), the token (constant time), the rate limit, the fields.
//! Only then is the clip handed to the app.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddrV4, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::http::{self, escape_html, json_string, Conn, HttpError, Request, Response};
use super::{clip_from_fields, tokens_match, Delivery, MAX_BODY_BYTES, PATH, TOKEN_HEADER};

/// Connections served at once; more get 503 right away.
pub const MAX_CONNECTIONS: usize = 4;
/// Time for a whole request to arrive.
pub const REQUEST_DEADLINE: Duration = Duration::from_secs(15);
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a connection waits for the app to save the clip.
pub const REPLY_WAIT: Duration = Duration::from_secs(10);
/// Accepted clips per minute.
pub const RATE_LIMIT: usize = 30;
/// How often the accept loop looks at the stop flag.
const POLL: Duration = Duration::from_millis(50);

/// A running listener. Dropping it (or `stop`) closes the port.
pub struct Server {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// What the connection threads share.
pub(super) struct Shared {
    pub port: u16,
    pub token: String,
    pub deliver: mpsc::Sender<Delivery>,
    pub active: AtomicUsize,
    pub recent: Mutex<VecDeque<Instant>>,
}

impl Server {
    /// Listen on 127.0.0.1:`port` (0: any free port, for tests). Clips go
    /// to `deliver`; requests must carry `token`.
    pub fn start(port: u16, token: String, deliver: mpsc::Sender<Delivery>) -> io::Result<Server> {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let shared = Arc::new(Shared {
            port,
            token,
            deliver,
            active: AtomicUsize::new(0),
            recent: Mutex::new(VecDeque::new()),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = thread::Builder::new()
            .name("notesec-clipper".into())
            .spawn(move || accept_loop(listener, shared, flag))?;
        Ok(Server {
            port,
            stop,
            thread: Some(thread),
        })
    }

    /// The port it listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Stop accepting and close the port (waits at most one poll).
    /// Requests already being served finish on their own threads.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Counts a connection while it is served.
struct Slot(Arc<Shared>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

fn accept_loop(listener: TcpListener, shared: Arc<Shared>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::SeqCst) {
        let (mut stream, peer) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(_) => {
                thread::sleep(POLL);
                continue;
            }
        };
        // Bound to 127.0.0.1, so this always holds; checked anyway.
        if !peer.ip().is_loopback() || stream.set_nonblocking(false).is_err() {
            continue;
        }
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
        if shared.active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            shared.active.fetch_sub(1, Ordering::SeqCst);
            let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
            let busy = text_error(HttpError(503, "busy, try again")).header("Retry-After", "2");
            finish(&mut stream, &busy);
            continue;
        }
        let slot = Slot(shared.clone());
        let _ = thread::Builder::new()
            .name("notesec-clip".into())
            .spawn(move || {
                let slot = slot;
                let response = respond(&mut stream, &slot.0, Instant::now() + REQUEST_DEADLINE);
                finish(&mut stream, &response);
            });
    }
}

fn finish(stream: &mut TcpStream, response: &Response) {
    let _ = stream.write_all(&response.to_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Both);
}

/// How the client wants its answer.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    /// A form post from the bookmarklet: a small HTML page.
    Form,
    Json,
}

fn kind(request: &Request) -> Option<Kind> {
    let media = request
        .header("content-type")?
        .split(';')
        .next()?
        .trim()
        .to_ascii_lowercase();
    match media.as_str() {
        "application/x-www-form-urlencoded" => Some(Kind::Form),
        "application/json" => Some(Kind::Json),
        _ => None,
    }
}

/// `Host` names this listener: `127.0.0.1:<port>` or `localhost:<port>`.
/// Anything else (a DNS-rebinding page's own name, no port) is refused.
pub(super) fn host_ok(host: Option<&str>, port: u16) -> bool {
    host.is_some_and(|h| {
        let h = h.to_ascii_lowercase();
        h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}")
    })
}

/// CORS: any origin may call (the token is the key), never with
/// credentials (no cookies here anyway).
fn cors(response: Response) -> Response {
    response.header("Access-Control-Allow-Origin", "*")
}

fn text_error(HttpError(status, why): HttpError) -> Response {
    let mut r = cors(Response::new(
        status,
        "text/plain; charset=utf-8",
        format!("{status} {}: {why}\n", http::reason(status)),
    ));
    if status == 405 {
        r = r.header("Allow", "POST, OPTIONS");
    }
    r
}

fn error(kind: Kind, status: u16, why: &str) -> Response {
    match kind {
        Kind::Json => cors(Response::new(
            status,
            "application/json",
            format!("{{\"ok\":false,\"error\":{}}}", json_string(why)),
        )),
        Kind::Form => page(
            status,
            "Not saved",
            &format!("NoteSec refused the clip: {why}."),
            false,
        ),
    }
}

/// The form post's answer: a self-contained page, nothing from outside,
/// its only script (closing the window) allowed by a fresh nonce, the
/// clipped title escaped.
fn page(status: u16, heading: &str, text: &str, close: bool) -> Response {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let script = if close {
        format!("<script nonce=\"{nonce}\">setTimeout(function(){{window.close()}},1500)</script>")
    } else {
        String::new()
    };
    let body = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>NoteSec</title>\
<style nonce=\"{nonce}\">body{{font:16px system-ui,sans-serif;margin:2em;text-align:center;color:#222}}\
h1{{font-size:1.3em}}p{{color:#666}}</style></head><body><h1>{}</h1><p>{}</p>{}{script}</body></html>",
        escape_html(heading),
        escape_html(text),
        if close { "<p>This window closes itself.</p>" } else { "" },
    );
    let csp = format!(
        "default-src 'none'; style-src 'nonce-{nonce}'; script-src 'nonce-{nonce}'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    );
    Response::new(status, "text/html; charset=utf-8", body)
        .header("Content-Security-Policy", csp)
        .header("X-Frame-Options", "DENY")
}

/// Read and answer one request on `conn`.
pub(super) fn respond(conn: &mut dyn Conn, shared: &Shared, deadline: Instant) -> Response {
    let port = shared.port;
    let token = shared.token.as_str();
    let mut check = |r: &Request| -> Result<(), HttpError> {
        if !host_ok(r.header("host"), port) {
            return Err(HttpError(
                403,
                "unknown Host: use 127.0.0.1:<port> or localhost:<port>",
            ));
        }
        if r.path != PATH {
            return Err(HttpError(404, "not found"));
        }
        match r.method.as_str() {
            "OPTIONS" => Ok(()),
            "POST" => {
                if kind(r).is_none() {
                    return Err(HttpError(
                        415,
                        "send application/x-www-form-urlencoded or application/json",
                    ));
                }
                if r.header(TOKEN_HEADER)
                    .is_some_and(|t| !tokens_match(t, token))
                {
                    return Err(HttpError(403, "wrong token"));
                }
                Ok(())
            }
            _ => Err(HttpError(405, "use POST")),
        }
    };
    let request = match http::read_request(conn, MAX_BODY_BYTES, deadline, &mut check) {
        Ok(r) => r,
        Err(err) => return text_error(err),
    };
    if request.method == "OPTIONS" {
        let mut r = cors(Response::new(204, "text/plain", Vec::new()))
            .header("Access-Control-Allow-Methods", "POST, OPTIONS")
            .header(
                "Access-Control-Allow-Headers",
                "Content-Type, X-NoteSec-Token",
            )
            .header("Access-Control-Max-Age", "600");
        if request
            .header("access-control-request-private-network")
            .is_some_and(|v| v.eq_ignore_ascii_case("true"))
        {
            r = r.header("Access-Control-Allow-Private-Network", "true");
        }
        return r;
    }
    let kind = kind(&request).unwrap_or(Kind::Form);
    let fields: HashMap<String, String> = match kind {
        Kind::Form => http::parse_form(&request.body),
        Kind::Json => match http::parse_json(&request.body) {
            Ok(f) => f,
            Err(why) => return error(kind, 400, why),
        },
    };
    let given = request
        .header(TOKEN_HEADER)
        .or(fields.get("token").map(String::as_str))
        .unwrap_or("");
    if !tokens_match(given, token) {
        return error(kind, 403, "wrong or missing token");
    }
    {
        let mut recent = shared.recent.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        while recent
            .front()
            .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60))
        {
            recent.pop_front();
        }
        if recent.len() >= RATE_LIMIT {
            return error(kind, 429, "too many clips, wait a minute").header("Retry-After", "60");
        }
        recent.push_back(now);
    }
    let clip = match clip_from_fields(&fields) {
        Ok(clip) => clip,
        Err(why) => return error(kind, 400, why),
    };
    let (reply, answer) = mpsc::channel();
    if shared.deliver.send(Delivery { clip, reply }).is_err() {
        return error(kind, 503, "NoteSec is closing");
    }
    match answer.recv_timeout(REPLY_WAIT) {
        Ok(Ok(title)) => match kind {
            Kind::Json => cors(Response::new(
                200,
                "application/json",
                format!("{{\"ok\":true,\"title\":{}}}", json_string(&title)),
            )),
            Kind::Form => page(
                200,
                "Saved to NoteSec \u{2713}",
                &format!("\u{201c}{title}\u{201d}"),
                true,
            ),
        },
        Ok(Err(why)) => error(kind, 500, &why),
        Err(_) => error(kind, 503, "NoteSec didn't answer in time"),
    }
}
