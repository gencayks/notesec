//! OpenAI-compatible AI helpers (decision 42): Ask my notes and later
//! features talk to a model through one HTTP path with three explicit
//! modes, chosen in Settings > AI:
//!
//! - **Local** (default): a server on this computer (LM Studio, Ollama,
//!   llama.cpp, vLLM, …) at a configurable `http://` URL. Plain HTTP is
//!   allowed only to loopback hosts that also *resolve* to loopback; the
//!   ureq agent disables proxies and redirects so vault text cannot leave.
//! - **API key**: the user's own key and base URL (OpenAI / Anthropic /
//!   xAI OpenAI-compatible). HTTPS required except for loopback (tests /
//!   a local TLS-less proxy). The key is sent as `Authorization: Bearer`
//!   and never appears in error text. Settings warns that vault content
//!   goes to that third party; the key lives in plaintext in `state.toml`.
//! - **Off**: every AI call returns a clear "AI is off" error.
//!
//! Modes never fall back into each other: an unreachable local server is
//! an error that names it, not a silent cloud call. The HTTP client is
//! `ureq` 3 (rustls). All calls block, so the app runs them on GPUI's
//! background executor. Nothing here knows about GPUI.

use crate::model::Page;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::{BufRead, BufReader};
use std::net::{IpAddr, ToSocketAddrs};
use std::time::Duration;
use ureq::Agent;

/// LM Studio's default server address (Local mode).
pub const DEFAULT_ENDPOINT: &str = "http://localhost:1234/v1";

/// A server that isn't running refuses at once; one that hangs gets this
/// long to accept.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Longest wait for the next bytes of a response. Local models can take a
/// while before the first token (loading the model, a long prompt).
const READ_TIMEOUT: Duration = Duration::from_secs(180);
/// Longest response body read into memory (embeddings of a big batch are
/// the largest).
const MAX_BODY: u64 = 256 << 20;

/// Which AI backend Settings has selected. Never switches on its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AiProvider {
    /// Local OpenAI-compatible server (LM Studio, …). The default.
    #[default]
    Local,
    /// Cloud (or remote) API with the user's own key.
    Api,
    /// AI features are disabled.
    Off,
}

impl AiProvider {
    pub fn label(self) -> &'static str {
        match self {
            AiProvider::Local => "Local",
            AiProvider::Api => "API key",
            AiProvider::Off => "Off",
        }
    }
}

/// Why a call to the model failed, worded for the user. Never contains an
/// API key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AiError {
    /// The endpoint isn't a URL we can use.
    BadUrl(String),
    /// Local mode: the endpoint isn't on this computer.
    NotLocal(String),
    /// API mode: the base URL isn't https (and isn't loopback http).
    NeedsHttps(String),
    /// API mode with an empty key.
    NoKey,
    /// AI is switched off in Settings.
    Off,
    /// Nothing answered at the endpoint (server off, wrong port, timeout).
    Unreachable(String),
    /// The server answered with an error status.
    Http(u16, String),
    /// The server's answer wasn't what an OpenAI-style API returns.
    BadResponse(String),
    /// No model is configured and the server lists none.
    NoModel,
}

impl fmt::Display for AiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AiError::BadUrl(why) => write!(f, "The AI endpoint isn't a usable URL: {why}"),
            AiError::NotLocal(host) => write!(
                f,
                "Local mode only talks to this computer (localhost), not {host}. \
                 Use API key mode for a remote server (your notes will leave this computer)"
            ),
            AiError::NeedsHttps(url) => {
                write!(f, "API key mode needs an https:// base URL (got {url})")
            }
            AiError::NoKey => write!(
                f,
                "No API key in Settings > AI. Paste your key, or switch to Local / Off"
            ),
            AiError::Off => write!(
                f,
                "AI is off. Turn it on in Settings > AI (Local or API key)"
            ),
            AiError::Unreachable(url) => {
                write!(f, "Can't reach the model server at {url}. Is it running?")
            }
            AiError::Http(status, msg) => write!(f, "The model server said {status}: {msg}"),
            AiError::BadResponse(why) => {
                write!(f, "Unexpected answer from the model server: {why}")
            }
            AiError::NoModel => write!(
                f,
                "No model is loaded. Load one on the server or pick one in Settings > AI"
            ),
        }
    }
}

/// A checked endpoint ready to call: base URL (no trailing slash), optional
/// bearer token (API mode only), and whether Local's loopback rules apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub base: String,
    pub bearer: Option<String>,
    pub local: bool,
}

/// True for names and addresses that mean this computer.
fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Split `http(s)://authority/path` into (scheme, host, port_str, path).
fn split_url(url: &str) -> Result<(&str, &str, &str, &str), AiError> {
    let url = url.trim();
    let (scheme, rest) = if let Some(r) = url.strip_prefix("https://") {
        ("https", r)
    } else if let Some(r) = url.strip_prefix("http://") {
        ("http", r)
    } else {
        return Err(AiError::BadUrl(
            "it should look like http://localhost:1234/v1 or https://api.example.com/v1".into(),
        ));
    };
    let (authority, path) = match rest.find(['/', '?', '#']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if path.contains(['?', '#']) {
        return Err(AiError::BadUrl("no ? or # parts, please".into()));
    }
    if authority.contains('@') {
        return Err(AiError::BadUrl("no user name in the URL, please".into()));
    }
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        let end = inner
            .find(']')
            .ok_or_else(|| AiError::BadUrl("unclosed [ in the address".into()))?;
        (&inner[..end], &inner[end + 1..])
    } else {
        match authority.rsplit_once(':') {
            Some((host, _)) => (host, &authority[host.len()..]),
            None => (authority, ""),
        }
    };
    if host.is_empty() {
        return Err(AiError::BadUrl("no host".into()));
    }
    // Validate the port if present.
    match port.strip_prefix(':') {
        None if port.is_empty() => {}
        Some(p) => {
            p.parse::<u16>()
                .map_err(|_| AiError::BadUrl(format!("bad port {p:?}")))?;
        }
        None => return Err(AiError::BadUrl(format!("bad address {authority:?}"))),
    }
    Ok((scheme, host, port, path))
}

impl Endpoint {
    /// Build an endpoint for the active provider. `api_key` is only used in
    /// API mode; it is never stored on the returned value in Local / Off.
    pub fn from_settings(
        provider: AiProvider,
        local_url: &str,
        api_base: &str,
        api_key: &str,
    ) -> Result<Endpoint, AiError> {
        match provider {
            AiProvider::Off => Err(AiError::Off),
            AiProvider::Local => Endpoint::local(local_url),
            AiProvider::Api => Endpoint::api(api_base, api_key),
        }
    }

    /// Local mode: `http://` to a loopback host only.
    pub fn local(url: &str) -> Result<Endpoint, AiError> {
        let (scheme, host, _port, path) = split_url(url)?;
        if scheme != "http" {
            return Err(AiError::BadUrl(
                "Local mode uses http:// (the local server doesn't need https)".into(),
            ));
        }
        if !is_loopback_host(host) {
            return Err(AiError::NotLocal(host.to_string()));
        }
        Ok(Endpoint {
            base: format!(
                "http://{}{}",
                authority_display(host, url),
                path.trim_end_matches('/')
            ),
            bearer: None,
            local: true,
        })
    }

    /// API mode: `https://` (or `http://` to loopback for tests), with a key.
    pub fn api(url: &str, key: &str) -> Result<Endpoint, AiError> {
        let key = key.trim();
        if key.is_empty() {
            return Err(AiError::NoKey);
        }
        let (scheme, host, _port, path) = split_url(url)?;
        let loopback = is_loopback_host(host);
        if scheme == "http" && !loopback {
            return Err(AiError::NeedsHttps(url.trim().to_string()));
        }
        if scheme != "http" && scheme != "https" {
            return Err(AiError::BadUrl("use https://".into()));
        }
        Ok(Endpoint {
            base: format!(
                "{scheme}://{}{}",
                authority_display(host, url),
                path.trim_end_matches('/')
            ),
            bearer: Some(key.to_string()),
            local: false,
        })
    }

    /// The endpoint as a URL, for messages (never includes the key).
    pub fn url(&self) -> &str {
        &self.base
    }

    /// For Local: refuse if any resolved address isn't loopback.
    fn check_resolved(&self) -> Result<(), AiError> {
        if !self.local {
            return Ok(());
        }
        let (_, host, port, _) = split_url(&self.base)?;
        let port: u16 = match port.strip_prefix(':') {
            None | Some("") => 80,
            Some(p) => p.parse().unwrap_or(80),
        };
        let addrs: Vec<_> = (host, port)
            .to_socket_addrs()
            .map_err(|_| AiError::Unreachable(self.base.clone()))?
            .collect();
        if addrs.is_empty() {
            return Err(AiError::Unreachable(self.base.clone()));
        }
        if addrs.iter().any(|a| !a.ip().is_loopback()) {
            return Err(AiError::NotLocal(host.to_string()));
        }
        Ok(())
    }
}

/// Re-read the authority (host[:port]) from the original URL so IPv6
/// brackets and explicit ports survive.
fn authority_display(host: &str, original: &str) -> String {
    let rest = original
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(host);
    authority.to_string()
}

/// A ureq agent for this endpoint: Local gets no proxy and no redirects.
fn agent(endpoint: &Endpoint) -> Agent {
    let mut builder = Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(READ_TIMEOUT))
        .timeout_recv_body(Some(READ_TIMEOUT));
    if endpoint.local {
        builder = builder.proxy(None);
    }
    builder.build().into()
}

fn apply_auth(
    req: ureq::RequestBuilder<ureq::typestate::WithoutBody>,
    endpoint: &Endpoint,
) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
    match &endpoint.bearer {
        Some(key) => req.header("Authorization", format!("Bearer {key}")),
        None => req,
    }
}

fn apply_auth_body(
    req: ureq::RequestBuilder<ureq::typestate::WithBody>,
    endpoint: &Endpoint,
) -> ureq::RequestBuilder<ureq::typestate::WithBody> {
    match &endpoint.bearer {
        Some(key) => req.header("Authorization", format!("Bearer {key}")),
        None => req,
    }
}

/// A transport error, worded for the user. Built from our own text only,
/// so neither the key nor request headers can end up in a message.
fn map_transport(err: ureq::Error, url: &str) -> AiError {
    match err {
        ureq::Error::Protocol(_) | ureq::Error::BodyExceedsLimit(_) => {
            AiError::BadResponse(format!("{url} doesn't answer like an HTTP API server"))
        }
        ureq::Error::BadUri(_) | ureq::Error::Http(_) => {
            AiError::BadUrl(format!("{url} isn't a valid URL"))
        }
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) => {
            AiError::BadResponse(format!("the secure (TLS) connection to {url} failed"))
        }
        _ => AiError::Unreachable(url.to_string()),
    }
}

/// The whole body as JSON; an error status becomes `AiError::Http`.
fn read_json(response: ureq::http::Response<ureq::Body>, url: &str) -> Result<Value, AiError> {
    let status = response.status().as_u16();
    let mut body = response.into_body();
    let text = body
        .with_config()
        .limit(MAX_BODY)
        .read_to_string()
        .map_err(|_| AiError::Unreachable(url.to_string()))?;
    let value: Option<Value> = serde_json::from_str(&text).ok();
    if !(200..300).contains(&status) {
        let message = value
            .as_ref()
            .and_then(error_message)
            .unwrap_or_else(|| text.trim().chars().take(200).collect());
        return Err(AiError::Http(status, message));
    }
    value.ok_or_else(|| AiError::BadResponse("the body isn't JSON".into()))
}

/// `{"error": {"message": ...}}` or `{"error": "..."}`.
fn error_message(value: &Value) -> Option<String> {
    let error = value.get("error")?;
    error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .map(str::to_string)
}

fn get_json(endpoint: &Endpoint, path: &str) -> Result<Value, AiError> {
    endpoint.check_resolved()?;
    let url = format!("{}{path}", endpoint.base);
    let agent = agent(endpoint);
    let req = apply_auth(agent.get(&url), endpoint);
    let response = req.call().map_err(|e| map_transport(e, endpoint.url()))?;
    read_json(response, endpoint.url())
}

fn post_json(
    endpoint: &Endpoint,
    path: &str,
    body: &Value,
) -> Result<ureq::http::Response<ureq::Body>, AiError> {
    endpoint.check_resolved()?;
    let url = format!("{}{path}", endpoint.base);
    let agent = agent(endpoint);
    let req = apply_auth_body(agent.post(&url), endpoint)
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json");
    req.send_json(body)
        .map_err(|e| map_transport(e, endpoint.url()))
}

// --- API calls ------------------------------------------------------------------

/// The ids of the models the server offers (`GET /models`).
pub fn list_models(endpoint: &Endpoint) -> Result<Vec<String>, AiError> {
    let value = get_json(endpoint, "/models")?;
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| AiError::BadResponse("no model list".into()))?;
    Ok(data
        .iter()
        .filter_map(|m| m.get("id").and_then(Value::as_str))
        .map(str::to_string)
        .collect())
}

/// True for model ids that look like embedding models (LM Studio lists
/// them next to chat models).
pub fn is_embedding_model(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    id.contains("embed") || id.contains("bge-") || id.contains("e5-")
}

/// The chat model to use: the configured one, else the first non-embedding
/// model the server lists.
pub fn chat_model(endpoint: &Endpoint, configured: &str) -> Result<String, AiError> {
    if !configured.trim().is_empty() {
        return Ok(configured.trim().to_string());
    }
    list_models(endpoint)?
        .into_iter()
        .find(|id| !is_embedding_model(id))
        .ok_or(AiError::NoModel)
}

/// A streamed chat completion: `next` gives the answer piece by piece.
pub struct ChatStream {
    body: Option<BufReader<ureq::BodyReader<'static>>>,
    /// A server that ignored `"stream": true` answered with one JSON body:
    /// its whole answer, given out once.
    whole: Option<String>,
}

impl ChatStream {
    /// The next piece of the answer; `None` when it is complete.
    pub fn next(&mut self) -> Result<Option<String>, AiError> {
        if let Some(whole) = self.whole.take() {
            return Ok(Some(whole).filter(|s| !s.is_empty()));
        }
        let Some(body) = self.body.as_mut() else {
            return Ok(None);
        };
        let mut line = String::new();
        loop {
            line.clear();
            let n = body
                .read_line(&mut line)
                .map_err(|_| AiError::BadResponse("the answer stopped half way".into()))?;
            if n == 0 {
                return Ok(None);
            }
            let Some(data) = line.trim_end().strip_prefix("data:") else {
                continue; // blank separators, comments, `event:` lines
            };
            let data = data.trim();
            if data == "[DONE]" {
                return Ok(None);
            }
            let Ok(event) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            if let Some(message) = error_message(&event) {
                return Err(AiError::BadResponse(message));
            }
            let piece = event
                .pointer("/choices/0/delta/content")
                .and_then(Value::as_str)
                .unwrap_or("");
            if !piece.is_empty() {
                return Ok(Some(piece.to_string()));
            }
        }
    }
}

/// Start a streamed chat completion with `messages` (OpenAI format).
pub fn chat_stream(
    endpoint: &Endpoint,
    model: &str,
    messages: Value,
) -> Result<ChatStream, AiError> {
    let body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "temperature": 0.2,
    });
    let response = post_json(endpoint, "/chat/completions", &body)?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let event_stream = content_type.starts_with("text/event-stream");
    if event_stream && (200..300).contains(&status) {
        let reader = response.into_body().into_reader();
        return Ok(ChatStream {
            body: Some(BufReader::new(reader)),
            whole: None,
        });
    }
    let value = read_json(response, endpoint.url())?;
    let whole = value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .ok_or_else(|| AiError::BadResponse("no answer in the reply".into()))?
        .to_string();
    Ok(ChatStream {
        body: None,
        whole: Some(whole),
    })
}

// --- Ask my notes -----------------------------------------------------------------

/// How many blocks an answer gets as context.
pub const ASK_SOURCES: usize = 8;
/// Longest block text (in characters) put into a prompt.
const SOURCE_CHARS: usize = 1500;

/// Words that say nothing about what a question is about.
const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "can", "did", "do", "does", "for",
    "from", "had", "has", "have", "how", "i", "if", "in", "is", "it", "its", "me", "my", "of",
    "on", "or", "our", "so", "that", "the", "their", "them", "there", "these", "this", "to", "was",
    "we", "were", "what", "when", "where", "which", "who", "why", "will", "with", "you", "your",
    "about", "any", "all", "tell", "should", "would", "could", "than", "then", "into",
];

/// The searchable words of `text`: lowercase runs of letters and digits,
/// stop words dropped, a plural "s" trimmed ("notes" finds "note").
pub fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        // Single letters ("s" of "Rust's", "a", "x") carry no meaning.
        .filter(|w| w.chars().count() > 1)
        .map(|w| {
            let w = w.to_lowercase();
            match w.strip_suffix('s') {
                Some(stem) if stem.chars().count() >= 3 && !stem.ends_with('s') => stem.to_string(),
                _ => w,
            }
        })
        .filter(|w| !STOP_WORDS.contains(&w.as_str()))
        .collect()
}

/// The blocks (as `(page, block)`) most likely to answer `question`, best
/// first, at most `limit`: a keyword ranking where rare words count more
/// (each question word scores `ln(1 + blocks / blocks with it)`), and a
/// word in the page's title counts half. Blocks that match nothing are
/// left out. Semantic search (decision 43) replaces this when embeddings
/// are available.
pub fn retrieve(pages: &[Page], question: &str, limit: usize) -> Vec<(usize, usize)> {
    let query: HashSet<String> = words(question).into_iter().collect();
    if query.is_empty() {
        return Vec::new();
    }
    let blocks: Vec<(usize, usize, HashSet<String>)> = pages
        .iter()
        .enumerate()
        .flat_map(|(p, page)| {
            page.blocks
                .iter()
                .enumerate()
                .filter_map(move |(b, block)| {
                    let w: HashSet<String> = words(&block.content).into_iter().collect();
                    (!w.is_empty()).then_some((p, b, w))
                })
        })
        .collect();
    let total = blocks.len().max(1) as f64;
    let mut df: HashMap<&str, usize> = HashMap::new();
    for (_, _, w) in &blocks {
        for word in w {
            if query.contains(word) {
                *df.entry(word.as_str()).or_default() += 1;
            }
        }
    }
    let idf = |word: &str| (1.0 + total / df.get(word).copied().unwrap_or(0).max(1) as f64).ln();
    let titles: Vec<HashSet<String>> = pages
        .iter()
        .map(|p| words(&p.title).into_iter().collect())
        .collect();
    let mut scored: Vec<(f64, usize, usize)> = blocks
        .iter()
        .filter_map(|(p, b, w)| {
            let score: f64 = query
                .iter()
                .map(|q| {
                    if w.contains(q) {
                        idf(q)
                    } else if titles[*p].contains(q) {
                        idf(q) / 2.0
                    } else {
                        0.0
                    }
                })
                .sum();
            (score > 0.0).then_some((score, *p, *b))
        })
        .collect();
    // Stable: equal scores keep page and document order.
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, p, b)| (p, b))
        .collect()
}

/// The chat messages for a question: the numbered sources (page title and
/// block text) and how to cite them.
pub fn ask_messages(question: &str, sources: &[(String, String)]) -> Value {
    let mut notes = String::new();
    for (i, (title, text)) in sources.iter().enumerate() {
        let text: String = text.chars().take(SOURCE_CHARS).collect();
        notes.push_str(&format!("[{}] (page \"{title}\")\n{text}\n\n", i + 1));
    }
    if sources.is_empty() {
        notes.push_str("(no notes matched the question)\n\n");
    }
    json!([
        {
            "role": "system",
            "content": "You answer questions about the user's own notes. Use only the numbered notes given. \
                        After each statement, cite the notes it comes from like [1] or [2][3]. \
                        If the notes don't answer the question, say so in one sentence. Be concise."
        },
        {
            "role": "user",
            "content": format!("Notes:\n\n{notes}Question: {question}")
        }
    ])
}

/// The source numbers (1-based) an answer cites with `[n]` or `[n, m]`, in
/// order of first citation, only those in `1..=count`.
pub fn cited(answer: &str, count: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let mut rest = answer;
    while let Some(open) = rest.find('[') {
        rest = &rest[open + 1..];
        let Some(close) = rest.find(']') else { break };
        let inside = &rest[..close];
        let numbers: Option<Vec<usize>> = inside
            .split(',')
            .map(|n| n.trim().parse::<usize>().ok())
            .collect();
        if let Some(numbers) = numbers {
            for n in numbers {
                if (1..=count).contains(&n) && !out.contains(&n) {
                    out.push(n);
                }
            }
        }
    }
    out
}

/// A fake model server for tests: answers each connection, in order, with
/// the next canned response, and records what it was sent (request line,
/// headers of interest, body).
#[cfg(test)]
pub mod test_server {
    use super::Endpoint;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

    /// A JSON response with `Content-Length`.
    pub fn json(status: u16, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    /// A chunked event stream with one chat delta per piece, then `[DONE]`.
    pub fn stream(pieces: &[&str]) -> String {
        let mut events = String::new();
        // A role-only delta first, as LM Studio sends.
        events.push_str("data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n");
        for piece in pieces {
            let event = serde_json::json!({"choices": [{"delta": {"content": piece}}]});
            events.push_str(&format!("data: {event}\n\n"));
        }
        events.push_str("data: [DONE]\n\n");
        // Split into small chunks to exercise de-chunking.
        let mut body = String::new();
        let bytes = events.as_bytes();
        for chunk in bytes.chunks(37) {
            body.push_str(&format!("{:x}\r\n", chunk.len()));
            body.push_str(std::str::from_utf8(chunk).unwrap_or(""));
            body.push_str("\r\n");
        }
        body.push_str("0\r\n\r\n");
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{body}"
        )
    }

    /// What the server was sent: request line, Authorization header (if
    /// any), and body of each request.
    pub type Requests = Arc<Mutex<Vec<(String, Option<String>, String)>>>;

    /// Serve `responses` on a free loopback port; the endpoint is
    /// `http://127.0.0.1:<port>/v1`.
    pub fn serve(responses: Vec<String>) -> (Endpoint, Requests) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests: Requests = Arc::default();
        let seen = requests.clone();
        thread::spawn(move || {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap_or(0);
                let mut length = 0;
                let mut authorization = None;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                    if lower.starts_with("authorization:") {
                        authorization = Some(line["authorization:".len()..].trim().to_string());
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap_or(());
                seen.lock().unwrap().push((
                    first.trim().to_string(),
                    authorization,
                    String::from_utf8_lossy(&body).into(),
                ));
                let _ = stream.write_all(response.as_bytes());
            }
        });
        let endpoint = Endpoint {
            base: format!("http://127.0.0.1:{port}/v1"),
            bearer: None,
            local: true,
        };
        (endpoint, requests)
    }

    /// An endpoint where nothing listens.
    pub fn dead_endpoint() -> Endpoint {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        Endpoint {
            base: format!("http://127.0.0.1:{port}/v1"),
            bearer: None,
            local: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_server::{dead_endpoint, json, serve, stream};
    use super::*;

    #[test]
    fn local_endpoints_must_be_loopback_http() {
        assert_eq!(
            Endpoint::local(" http://localhost:1234/v1/ ").unwrap().base,
            "http://localhost:1234/v1"
        );
        let ep = Endpoint::local("http://[::1]:8080").unwrap();
        assert_eq!(ep.base, "http://[::1]:8080");
        assert!(ep.local && ep.bearer.is_none());
        assert_eq!(
            Endpoint::local("http://127.0.0.2/api").unwrap().base,
            "http://127.0.0.2/api"
        );
        for remote in [
            "http://192.168.1.10:1234/v1",
            "http://example.com/v1",
            "http://localhost.evil.com:1234",
        ] {
            assert!(
                matches!(Endpoint::local(remote), Err(AiError::NotLocal(_))),
                "{remote}"
            );
        }
        for bad in [
            "https://localhost:1234/v1",
            "localhost:1234",
            "http://localhost:x/v1",
            "http://user@localhost/v1",
            "http://localhost/v1?key=1",
            "http://:1234",
        ] {
            assert!(
                matches!(Endpoint::local(bad), Err(AiError::BadUrl(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn api_endpoints_need_https_unless_loopback_and_a_key() {
        assert_eq!(
            Endpoint::api("https://api.openai.com/v1", "sk-test")
                .unwrap()
                .base,
            "https://api.openai.com/v1"
        );
        let loopback = Endpoint::api("http://127.0.0.1:9/v1", "secret").unwrap();
        assert_eq!(loopback.bearer.as_deref(), Some("secret"));
        assert!(!loopback.local);
        assert!(matches!(
            Endpoint::api("http://api.openai.com/v1", "sk"),
            Err(AiError::NeedsHttps(_))
        ));
        assert_eq!(Endpoint::api("https://x.com/v1", "  "), Err(AiError::NoKey));
        assert_eq!(
            Endpoint::from_settings(AiProvider::Off, "", "", ""),
            Err(AiError::Off)
        );
    }

    #[test]
    fn lists_models_and_picks_a_chat_model() {
        let models = r#"{"data":[{"id":"text-embedding-nomic"},{"id":"qwen2.5-7b"}]}"#;
        let (ep, requests) = serve(vec![json(200, models), json(200, models)]);
        assert_eq!(
            list_models(&ep).unwrap(),
            vec!["text-embedding-nomic", "qwen2.5-7b"]
        );
        assert_eq!(chat_model(&ep, "").unwrap(), "qwen2.5-7b");
        assert_eq!(chat_model(&ep, " mine ").unwrap(), "mine", "no request");
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].0, "GET /v1/models HTTP/1.1");
        assert_eq!(requests[0].1, None, "local mode sends no Authorization");
    }

    #[test]
    fn api_mode_sends_bearer_and_local_does_not() {
        let models = r#"{"data":[{"id":"m"}]}"#;
        let (mut ep, requests) = serve(vec![json(200, models)]);
        ep.bearer = Some("sk-secret-key".into());
        ep.local = false;
        list_models(&ep).unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].1.as_deref(), Some("Bearer sk-secret-key"));
    }

    #[test]
    fn streams_a_chat_answer_from_chunked_events() {
        let (ep, requests) = serve(vec![stream(&["Rust ", "is ", "fast [1]."])]);
        let mut answer = chat_stream(&ep, "m", json!([{"role": "user", "content": "hi"}])).unwrap();
        let mut pieces = Vec::new();
        while let Some(piece) = answer.next().unwrap() {
            pieces.push(piece);
        }
        assert_eq!(pieces, ["Rust ", "is ", "fast [1]."]);
        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].0, "POST /v1/chat/completions HTTP/1.1");
        let body: Value = serde_json::from_str(&requests[0].2).unwrap();
        assert_eq!(body["model"], "m");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn a_server_that_does_not_stream_still_answers() {
        let reply = r#"{"choices":[{"message":{"role":"assistant","content":"All at once."}}]}"#;
        let (ep, _) = serve(vec![json(200, reply)]);
        let mut answer = chat_stream(&ep, "m", json!([])).unwrap();
        assert_eq!(answer.next().unwrap().as_deref(), Some("All at once."));
        assert_eq!(answer.next().unwrap(), None);
    }

    #[test]
    fn errors_say_what_went_wrong() {
        let (ep, _) = serve(vec![
            json(404, r#"{"error":{"message":"model not found"}}"#),
            json(500, "oops"),
            "garbage\r\n\r\n".to_string(),
        ]);
        assert_eq!(
            chat_stream(&ep, "m", json!([])).err(),
            Some(AiError::Http(404, "model not found".into()))
        );
        assert_eq!(
            list_models(&ep).err(),
            Some(AiError::Http(500, "oops".into()))
        );
        assert!(matches!(list_models(&ep), Err(AiError::BadResponse(_))));
        let dead = dead_endpoint();
        let err = list_models(&dead).unwrap_err();
        assert_eq!(err, AiError::Unreachable(dead.url().to_string()));
        assert!(err.to_string().contains("Can't reach"));
        assert!(!err.to_string().contains("sk-"), "errors never show a key");
        let none = r#"{"data":[{"id":"nomic-embed-text"}]}"#;
        let (ep, _) = serve(vec![json(200, none)]);
        assert_eq!(chat_model(&ep, ""), Err(AiError::NoModel));
    }

    #[test]
    fn words_drop_stop_words_and_plurals() {
        assert_eq!(
            words("What are my Notes about Rust's borrow-checker? 2024"),
            vec!["note", "rust", "borrow", "checker", "2024"]
        );
        // Short words and "ss" endings keep their s.
        assert_eq!(words("bus class glass"), vec!["bus", "class", "glass"]);
    }

    #[test]
    fn retrieve_ranks_rare_words_and_titles() {
        let pages = vec![
            Page::from_markdown("Rust", false, "- ownership and borrowing\n- the book\n"),
            Page::from_markdown("Diary", false, "- read the rust book today\n- weather\n"),
            Page::from_markdown("Misc", false, "- the book shelf\n- borrowing money\n"),
        ];
        let hits = retrieve(&pages, "How does borrowing work in Rust?", 10);
        // "rust" and "borrowing" both: Rust's first block (title counts for
        // rust); then single-word matches.
        assert_eq!(hits[0], (0, 0));
        assert!(hits.contains(&(1, 0)) && hits.contains(&(2, 1)));
        assert!(!hits.contains(&(1, 1)), "weather matches nothing");
        assert_eq!(retrieve(&pages, "how is the", 10), vec![]);
        assert_eq!(retrieve(&pages, "book", 2).len(), 2);
    }

    #[test]
    fn prompts_number_the_sources_and_citations_are_read_back() {
        let messages = ask_messages(
            "What is it?",
            &[("A".into(), "alpha".into()), ("B".into(), "beta".into())],
        );
        let user = messages[1]["content"].as_str().unwrap();
        assert!(user.contains("[1] (page \"A\")\nalpha"));
        assert!(user.contains("[2] (page \"B\")\nbeta"));
        assert!(user.ends_with("Question: What is it?"));
        assert!(messages[0]["content"].as_str().unwrap().contains("[1]"));

        assert_eq!(
            cited("Yes [2]. Also [1, 2] and [[Page]] [7] [x].", 3),
            vec![2, 1]
        );
        assert_eq!(cited("no citations", 3), Vec::<usize>::new());
    }
}
