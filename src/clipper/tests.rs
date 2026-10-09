//! The clipper without the app: tokens, titles, the page, every refusal,
//! and real requests to a listener on 127.0.0.1.

use super::http::tests::Fake;
use super::server::{respond, Shared, MAX_CONNECTIONS, RATE_LIMIT};
use super::*;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const TOKEN: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

/// What the app would do: answer each delivery, keeping the clips.
fn sink() -> (mpsc::Sender<Delivery>, Arc<Mutex<Vec<Clip>>>) {
    let (tx, rx) = mpsc::channel::<Delivery>();
    let clips = Arc::new(Mutex::new(Vec::new()));
    let kept = clips.clone();
    thread::spawn(move || {
        for delivery in rx {
            let title = delivery.clip.title.clone();
            kept.lock().unwrap().push(delivery.clip);
            let _ = delivery.reply.send(Ok(title));
        }
    });
    (tx, clips)
}

fn shared(port: u16) -> (Shared, Arc<Mutex<Vec<Clip>>>) {
    let (tx, clips) = sink();
    let shared = Shared {
        port,
        token: TOKEN.to_string(),
        deliver: tx,
        active: AtomicUsize::new(0),
        recent: Mutex::new(VecDeque::new()),
    };
    (shared, clips)
}

/// Answer `raw` through `respond`: (status, head, body).
fn ask(shared: &Shared, raw: &str) -> (u16, String, String) {
    let mut conn = Fake::new(raw);
    let response = respond(&mut conn, shared, Instant::now() + Duration::from_secs(5));
    let bytes = response.to_bytes();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    (response.status, head.to_string(), body.to_string())
}

fn post(host: &str, content_type: &str, extra: &str, body: &str) -> String {
    format!(
        "POST /clip HTTP/1.1\r\nHost: {host}\r\nContent-Type: {content_type}\r\n{extra}Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

const FORM: &str = "application/x-www-form-urlencoded";

#[test]
fn tokens_titles_and_the_page() {
    let (a, b) = (new_token(), new_token());
    assert_eq!(a.len(), 64);
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(a, b);
    assert!(tokens_match(TOKEN, TOKEN));
    assert!(!tokens_match(&TOKEN[..63], TOKEN));
    assert!(!tokens_match(&format!("{TOKEN}0"), TOKEN));
    assert!(!tokens_match("", TOKEN));
    assert!(!tokens_match("", ""));
    let mut wrong = TOKEN.to_string();
    wrong.replace_range(63.., "0");
    assert!(!tokens_match(&wrong, TOKEN));

    assert_eq!(
        clip_title("  A [great]\n page / site | X ", None),
        "A (great) page - site | X"
    );
    assert_eq!(clip_title("..hidden", None), "hidden");
    assert_eq!(clip_title("a___b", None), "a_b");
    assert_eq!(
        clip_title("", Some("https://www.example.com/x")),
        "example.com"
    );
    assert_eq!(clip_title(" \u{0} ", None), "Clipped page");
    assert_eq!(clip_title(&"é".repeat(500), None).len(), 150);
    assert_eq!(unique_title("A", |t| t == "A" || t == "A (2)"), "A (3)");

    let clip = Clip {
        title: "T".into(),
        url: Some("https://e.com/a".into()),
        rows: vec![(0, "# H".into()), (1, "text".into())],
        truncated: true,
    };
    assert_eq!(
        clip_page(&clip, "T", "2026-10-09").to_markdown(),
        "- source:: https://e.com/a\n  clipped:: [[2026-10-09]]\n  tags:: #clipped\n- # H\n  - text\n- *(Clip shortened: the page was longer than NoteSec keeps.)*\n"
    );
    let bm = bookmarklet(27183, "ab'c;d");
    assert!(bm.starts_with("javascript:(function(){"));
    assert!(bm.contains("token:'abcd'"));
    assert!(bm.contains("f.action='http://127.0.0.1:27183/clip'"));
    assert!(!bm.contains('%') && !bm.contains('\n'));
}

#[test]
fn refusals_come_before_anything_is_saved() {
    let (s, clips) = shared(5000);
    let body = format!("token={TOKEN}&title=T&html=x");
    // DNS rebinding: a page on evil.example resolving to 127.0.0.1.
    for host in [
        "evil.example:5000",
        "127.0.0.1",
        "127.0.0.1:5001",
        "localhost",
        "[::1]:5000",
        "127.0.0.1:5000.evil",
    ] {
        let (status, _, body) = ask(&s, &post(host, FORM, "", &body));
        assert_eq!(status, 403, "{host}");
        assert!(body.contains("unknown Host"), "{body}");
    }
    let (status, _, _) = ask(
        &s,
        &format!("POST /clip HTTP/1.1\r\nContent-Type: {FORM}\r\nContent-Length: 1\r\n\r\nx"),
    );
    assert_eq!(status, 403, "no Host");
    let (status, ..) = ask(
        &s,
        &post("127.0.0.1:5000", FORM, "", &body).replace("/clip", "/other"),
    );
    assert_eq!(status, 404);
    let (status, head, _) = ask(&s, "GET /clip HTTP/1.1\r\nHost: 127.0.0.1:5000\r\n\r\n");
    assert_eq!(status, 405);
    assert!(head.contains("Allow: POST, OPTIONS"));
    let (status, ..) = ask(&s, &post("127.0.0.1:5000", "text/plain", "", &body));
    assert_eq!(status, 415);
    let (status, ..) = ask(
        &s,
        &post(
            "127.0.0.1:5000",
            "multipart/form-data; boundary=x",
            "",
            &body,
        ),
    );
    assert_eq!(status, 415);
    // A wrong token header is refused before the body is read.
    let (status, ..) = ask(
        &s,
        &post(
            "localhost:5000",
            "application/json",
            "X-NoteSec-Token: nope\r\n",
            "{",
        ),
    );
    assert_eq!(status, 403);
    // A wrong or missing token in the body: 403, as HTML for a form.
    let (status, head, page) = ask(
        &s,
        &post("127.0.0.1:5000", FORM, "", "token=nope&title=T&html=x"),
    );
    assert_eq!(status, 403);
    assert!(head.contains("Content-Security-Policy: default-src 'none'"));
    assert!(page.contains("Not saved") && !page.contains("<script"));
    let (status, _, json) = ask(
        &s,
        &post("127.0.0.1:5000", "application/json", "", r#"{"title":"T"}"#),
    );
    assert_eq!(
        (status, json.as_str()),
        (403, r#"{"ok":false,"error":"wrong or missing token"}"#)
    );
    let (status, ..) = ask(
        &s,
        &post("127.0.0.1:5000", "application/json", "", "{bad json"),
    );
    assert_eq!(status, 400);
    let (status, ..) = ask(
        &s,
        &post("127.0.0.1:5000", FORM, "", &format!("token={TOKEN}")),
    );
    assert_eq!(status, 400, "nothing to clip");
    let (status, ..) = ask(&s, &format!("POST /clip HTTP/1.1\r\nHost: 127.0.0.1:5000\r\nContent-Type: {FORM}\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"));
    assert_eq!(status, 501);
    let big = format!("POST /clip HTTP/1.1\r\nHost: 127.0.0.1:5000\r\nContent-Type: {FORM}\r\nContent-Length: {}\r\n\r\n", MAX_BODY_BYTES + 1);
    assert_eq!(ask(&s, &big).0, 413);
    assert!(clips.lock().unwrap().is_empty(), "nothing was delivered");

    // The CORS preflight: any origin, no credentials.
    let (status, head, _) = ask(&s, "OPTIONS /clip HTTP/1.1\r\nHost: 127.0.0.1:5000\r\nOrigin: https://site.example\r\nAccess-Control-Request-Method: POST\r\nAccess-Control-Request-Private-Network: true\r\n\r\n");
    assert_eq!(status, 204);
    for h in [
        "Access-Control-Allow-Origin: *",
        "Access-Control-Allow-Methods: POST, OPTIONS",
        "Access-Control-Allow-Headers: Content-Type, X-NoteSec-Token",
        "Access-Control-Allow-Private-Network: true",
    ] {
        assert!(head.contains(h), "{h} in {head}");
    }
    assert!(!head.contains("Credentials"));

    // Accepted: the form gets a closing page, its title escaped.
    let form = format!("token={TOKEN}&title=%3Cscript%3Ealert(1)%3C%2Fscript%3E&url=https%3A%2F%2Fe.com%2Fp&html=%3Cp%3Ehi%3C%2Fp%3E");
    let (status, head, page) = ask(&s, &post("127.0.0.1:5000", FORM, "", &form));
    assert_eq!(status, 200, "{page}");
    assert!(page.contains("Saved to NoteSec \u{2713}"));
    assert!(
        page.contains("&lt;script&gt;alert(1)&lt;-script&gt;"),
        "{page}"
    );
    assert_eq!(
        page.matches("<script").count(),
        1,
        "only the closing script: {page}"
    );
    let nonce = head
        .split("script-src 'nonce-")
        .nth(1)
        .unwrap()
        .split('\'')
        .next()
        .unwrap();
    assert!(page.contains(&format!("<script nonce=\"{nonce}\">")));
    assert!(
        head.contains("X-Frame-Options: DENY") && head.contains("X-Content-Type-Options: nosniff")
    );
    let json =
        format!(r#"{{"token":"{TOKEN}","title":"J","url":"https://e.com/j","html":"<h1>x</h1>"}}"#);
    let (status, head, body) = ask(
        &s,
        &post(
            "localhost:5000",
            "application/json; charset=utf-8",
            "",
            &json,
        ),
    );
    assert_eq!((status, body.as_str()), (200, r#"{"ok":true,"title":"J"}"#));
    assert!(head.contains("Access-Control-Allow-Origin: *"));
    let clips = clips.lock().unwrap().clone();
    assert_eq!(clips.len(), 2);
    assert_eq!(clips[0].title, "<script>alert(1)<-script>");
    assert_eq!(clips[0].url.as_deref(), Some("https://e.com/p"));
    assert_eq!(clips[0].rows, [(0, "hi".to_string())]);
    assert_eq!(clips[1].rows, [(0, "# x".to_string())]);

    // The rate limit counts accepted clips.
    {
        let mut recent = s.recent.lock().unwrap();
        recent.clear();
        recent.extend(std::iter::repeat_n(Instant::now(), RATE_LIMIT));
    }
    let (status, head, _) = ask(&s, &post("127.0.0.1:5000", "application/json", "", &json));
    assert_eq!(status, 429);
    assert!(head.contains("Retry-After: 60"));
}

/// Send `raw` to 127.0.0.1:`port`; the whole answer.
fn send(port: u16, raw: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(raw.as_bytes()).unwrap();
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer);
    answer
}

#[test]
fn a_real_listener_on_localhost() {
    let (tx, clips) = sink();
    let mut server = Server::start(0, TOKEN.to_string(), tx).unwrap();
    let port = server.port();
    assert_ne!(port, 0);
    let host = format!("127.0.0.1:{port}");

    let form = format!("token={TOKEN}&title=Form+clip&url=https%3A%2F%2Fe.com%2Fa&html=%3Cp%3Eform+%3Cb%3Ebody%3C%2Fb%3E%3C%2Fp%3E%3Cscript%3Ex()%3C%2Fscript%3E");
    let answer = send(port, &post(&host, FORM, "", &form));
    assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
    assert!(answer.contains("Saved to NoteSec"));
    let json = format!(
        r#"{{"title":"Json clip","url":"https://e.com/b","html":"<ul><li>one</li></ul>"}}"#
    );
    let answer = send(
        port,
        &post(
            &format!("localhost:{port}"),
            "application/json",
            &format!("X-NoteSec-Token: {TOKEN}\r\n"),
            &json,
        ),
    );
    assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
    assert!(answer.ends_with(r#"{"ok":true,"title":"Json clip"}"#));
    let bad = send(port, &post(&host, FORM, "", "token=wrong&title=Bad&html=x"));
    assert!(bad.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{bad}");
    let rebind = send(
        port,
        &post(&format!("attacker.example:{port}"), FORM, "", &form),
    );
    assert!(rebind.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{rebind}");
    {
        let clips = clips.lock().unwrap();
        let titles: Vec<&str> = clips.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, ["Form clip", "Json clip"]);
        assert_eq!(clips[0].rows, [(0, "form **body**".to_string())]);
    }

    // At most MAX_CONNECTIONS at once: idle ones hold their slots.
    let idle: Vec<TcpStream> = (0..MAX_CONNECTIONS)
        .map(|_| TcpStream::connect(("127.0.0.1", port)).unwrap())
        .collect();
    thread::sleep(Duration::from_millis(300));
    let busy = send(port, &post(&host, FORM, "", &form));
    assert!(
        busy.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
        "{busy}"
    );
    drop(idle);
    thread::sleep(Duration::from_millis(300));
    let again = send(port, &post(&host, FORM, "", &form));
    assert!(again.starts_with("HTTP/1.1 200 OK\r\n"), "{again}");

    // Stopped: the port is closed.
    server.stop();
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
    // And a port in use is an error, not a panic.
    let holder = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = holder.local_addr().unwrap().port();
    let (tx, _) = sink();
    let err = Server::start(taken, TOKEN.into(), tx).err().unwrap();
    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
}
