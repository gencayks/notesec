//! Community plugin marketplace (roadmap v0.3.0 feature 2,
//! docs/MARKETPLACE.md): a public `notesec-plugins` GitHub repo serves an
//! index (`index.toml`) of reviewed plugins; Settings > Plugins > Browse
//! fetches it and installs with one click. Every download is checked
//! against the index `sha256` and refused loudly on mismatch. No GPUI here.

use super::{valid_id, API_VERSION, MAX_WASM};
use std::time::Duration;

/// Where the Browse tab looks: `index.toml` at the root of the public
/// `notesec-plugins` registry repo.
pub const DEFAULT_REGISTRY_URL: &str =
    "https://raw.githubusercontent.com/gencayks/notesec-plugins/main/index.toml";

/// Biggest `index.toml` read into memory.
const MAX_INDEX: u64 = 256 << 10;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// One reviewed plugin in the registry index.
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryEntry {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub download_url: String,
    pub sha256: String,
    pub api_version: u32,
    #[serde(default)]
    pub commands: Vec<RegistryCommand>,
}

/// A palette command a registry plugin offers (same shape as the manifest).
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryCommand {
    pub id: String,
    pub label: String,
}

#[derive(serde::Deserialize)]
struct RawIndex {
    #[serde(default)]
    plugins: Vec<RegistryEntry>,
}

/// SHA-256 of `bytes`, lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Accept `entry`, or say why the registry listing is unusable. Mirrors the
/// manifest limits in `super::load` so an installed plugin always loads.
pub fn check_entry(entry: &RegistryEntry) -> Result<(), String> {
    if !valid_id(&entry.id) {
        return Err(format!(
            "registry plugin \u{201c}{}\u{201d}: bad id (a-z, 0-9, -, at most 48)",
            entry.id
        ));
    }
    let short =
        |s: &str| !s.trim().is_empty() && s.chars().count() <= 80 && !s.contains(['\n', '\r']);
    if !short(&entry.name) || !short(&entry.version) || !short(&entry.author) {
        return Err(format!(
            "registry plugin \u{201c}{}\u{201d}: name, version or author is empty or too long",
            entry.id
        ));
    }
    if entry.description.chars().count() > 500 || entry.description.contains(['\n', '\r']) {
        return Err(format!(
            "registry plugin \u{201c}{}\u{201d}: description is too long (at most 500 characters, one line)",
            entry.id
        ));
    }
    if entry.api_version != API_VERSION {
        return Err(format!(
            "registry plugin \u{201c}{}\u{201d}: it needs plugin API {}; this NoteSec has API {API_VERSION}",
            entry.id, entry.api_version
        ));
    }
    check_url(&entry.download_url).map_err(|why| {
        format!(
            "registry plugin \u{201c}{}\u{201d}: bad download URL: {why}",
            entry.id
        )
    })?;
    if !is_sha256(&entry.sha256) {
        return Err(format!(
            "registry plugin \u{201c}{}\u{201d}: sha256 isn't 64 hex digits",
            entry.id
        ));
    }
    if entry.commands.len() > super::MAX_COMMANDS {
        return Err(format!(
            "registry plugin \u{201c}{}\u{201d}: more than {} commands",
            entry.id,
            super::MAX_COMMANDS
        ));
    }
    for c in &entry.commands {
        if !valid_id(&c.id) || !short(&c.label) {
            return Err(format!(
                "registry plugin \u{201c}{}\u{201d}: command \u{201c}{}\u{201d} has a bad id or label",
                entry.id, c.id
            ));
        }
    }
    Ok(())
}

/// Only `https://` (plain `http://` to loopback, for tests).
fn check_url(url: &str) -> Result<(), String> {
    let url = url.trim();
    if let Some(rest) = url.strip_prefix("https://") {
        if rest.contains([' ', '\n', '\r']) || !rest.contains('.') && !rest.contains('/') {
            return Err("not a usable https URL".into());
        }
        return Ok(());
    }
    if let Some(rest) = url.strip_prefix("http://") {
        let host = rest.split(['/', '?', '#']).next().unwrap_or("");
        let host = host.strip_prefix('[').unwrap_or(host);
        if host == "localhost" || host == "127.0.0.1" || host == "::1" || host.starts_with("127.") {
            return Ok(());
        }
        return Err("plain http is only allowed to this computer (tests)".into());
    }
    Err("use an https:// URL".into())
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Parse and check an `index.toml` body: the `[[plugins]]` entries.
pub fn parse_index(text: &str) -> Result<Vec<RegistryEntry>, String> {
    let raw: RawIndex = toml::from_str(text)
        .map_err(|err| format!("the plugin index is bad TOML: {}", err.message()))?;
    let mut seen = std::collections::HashSet::new();
    for entry in &raw.plugins {
        check_entry(entry)?;
        if !seen.insert(entry.id.clone()) {
            return Err(format!(
                "the plugin index lists \u{201c}{}\u{201d} twice",
                entry.id
            ));
        }
    }
    let mut out = raw.plugins;
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(READ_TIMEOUT))
        .timeout_recv_body(Some(READ_TIMEOUT))
        .build()
        .into()
}

fn get_bytes(url: &str, max: u64) -> Result<(u16, Vec<u8>), String> {
    check_url(url).map_err(|why| format!("bad URL {url}: {why}"))?;
    let response = agent()
        .get(url)
        .call()
        .map_err(|_| format!("can't reach {url}: is the network up?"))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("{url} answered with status {status}"));
    }
    response
        .into_body()
        .with_config()
        .limit(max)
        .read_to_vec()
        .map(|bytes| (status, bytes))
        .map_err(|_| format!("{url} sent more than {max} bytes; refusing to read it"))
}

/// Fetch the registry index: every entry ready to list.
pub fn fetch_index(url: &str) -> Result<Vec<RegistryEntry>, String> {
    let (_, bytes) = get_bytes(url, MAX_INDEX)?;
    let text = String::from_utf8(bytes).map_err(|_| "the plugin index isn't UTF-8".to_string())?;
    parse_index(&text)
}

/// Download one plugin's WASM (size-capped like `super::MAX_WASM`).
pub fn download_wasm(url: &str) -> Result<Vec<u8>, String> {
    let (_, bytes) = get_bytes(url, MAX_WASM)?;
    if bytes.is_empty() {
        return Err(format!("{url} sent an empty file"));
    }
    Ok(bytes)
}

/// The downloaded bytes must hash to the index `sha256`. LOUD on mismatch:
/// a tampered download is never installed.
pub fn verify(entry: &RegistryEntry, bytes: &[u8]) -> Result<(), String> {
    let got = sha256_hex(bytes);
    if got != entry.sha256.to_ascii_lowercase() {
        return Err(format!(
            "REFUSED: plugin \u{201c}{}\u{201d} failed its sha256 check \
             (the download doesn't match the registry index — it may be tampered with, \
             so it was NOT installed). Expected {}, got {got}.",
            entry.id, entry.sha256
        ));
    }
    Ok(())
}

/// The `plugin.toml` an install writes, from the checked index entry.
pub fn manifest_for(entry: &RegistryEntry) -> String {
    let mut out = String::new();
    let field =
        |key: &str, value: &str| format!("{key} = {}\n", toml::Value::String(value.to_string()));
    out.push_str(&field("id", &entry.id));
    out.push_str(&field("name", &entry.name));
    out.push_str(&field("version", &entry.version));
    out.push_str(&format!("api_version = {}\n", entry.api_version));
    out.push_str(&field("description", &entry.description));
    for c in &entry.commands {
        out.push_str("\n[[commands]]\n");
        out.push_str(&field("id", &c.id));
        out.push_str(&field("label", &c.label));
    }
    out
}

/// Install `entry` (already checked) from `bytes` (already verified) into
/// `<root>/plugins/<id>/`, atomically (tmp file + rename, like vault
/// saves). The caller reloads and enables it.
pub fn install(root: &std::path::Path, entry: &RegistryEntry, bytes: &[u8]) -> Result<(), String> {
    check_entry(entry)?;
    verify(entry, bytes)?;
    if bytes.len() as u64 > MAX_WASM {
        return Err(format!(
            "plugin \u{201c}{}\u{201d} is too big ({} bytes, at most {MAX_WASM})",
            entry.id,
            bytes.len()
        ));
    }
    let dir = root.join("plugins").join(&entry.id);
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("can't install plugin \u{201c}{}\u{201d}: {err}", entry.id))?;
    let write_atomic = |name: &str, data: &[u8]| -> Result<(), String> {
        let path = dir.join(name);
        let tmp = dir.join(format!("{name}.tmp"));
        std::fs::write(&tmp, data)
            .map_err(|err| format!("can't install plugin \u{201c}{}\u{201d}: {err}", entry.id))?;
        std::fs::rename(&tmp, &path)
            .map_err(|err| format!("can't install plugin \u{201c}{}\u{201d}: {err}", entry.id))?;
        Ok(())
    };
    write_atomic("plugin.wasm", bytes)?;
    write_atomic("plugin.toml", manifest_for(entry).as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX: &str = r#"
[[plugins]]
id = "word-count"
name = "Word count"
version = "1.0.0"
description = "Counts words."
author = "NoteSec"
download_url = "https://example.com/word-count.wasm"
sha256 = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
api_version = 1

[[plugins.commands]]
id = "count"
label = "Count words in this block"
"#;

    #[test]
    fn sha256_matches_the_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_good_index_parses_and_sorts() {
        let entries = parse_index(INDEX).unwrap();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(
            (e.id.as_str(), e.author.as_str()),
            ("word-count", "NoteSec")
        );
        assert_eq!(e.commands.len(), 1);
    }

    #[test]
    fn bad_index_entries_are_rejected() {
        let bad_id = INDEX.replace("word-count", "Bad_Id");
        assert!(parse_index(&bad_id).is_err());
        let bad_sha = INDEX.replace("ba7816bf", "zzzzzzzz");
        assert!(parse_index(&bad_sha).unwrap_err().contains("sha256"));
        let bad_url = INDEX.replace("https://example.com", "http://example.com");
        assert!(parse_index(&bad_url).unwrap_err().contains("download URL"));
        let bad_api = INDEX.replace("api_version = 1", "api_version = 2");
        assert!(parse_index(&bad_api).unwrap_err().contains("API 2"));
        let dup = format!("{INDEX}\n{INDEX}");
        assert!(parse_index(&dup).unwrap_err().contains("twice"));
        assert!(parse_index("not toml [[[").is_err());
        let unknown = INDEX.replace("author = ", "evil = \"x\"\nauthor = ");
        assert!(parse_index(&unknown).is_err());
    }

    #[test]
    fn verification_is_loud_on_mismatch() {
        let entry = &parse_index(INDEX).unwrap()[0];
        assert!(verify(entry, b"abc").is_ok());
        let err = verify(entry, b"tampered").unwrap_err();
        assert!(err.contains("REFUSED"), "{err}");
        assert!(err.contains("NOT installed"), "{err}");
    }

    #[test]
    fn install_writes_a_loadable_plugin_and_refuses_tampered_bytes() {
        let root = std::env::temp_dir().join(format!("notesec-market-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let entry = &parse_index(INDEX).unwrap()[0];
        assert!(install(&root, entry, b"tampered").is_err());
        assert!(
            !root.join("plugins/word-count/plugin.wasm").exists(),
            "tampered bytes must leave nothing installed"
        );
        install(&root, entry, b"abc").unwrap();
        let plugin = super::super::load(&root.join("plugins/word-count")).unwrap();
        assert_eq!(plugin.id, "word-count");
        assert_eq!(plugin.commands.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn index_and_wasm_come_over_http() {
        use crate::ai::test_server::serve;
        let wasm: &[u8] = b"abc";
        // The wasm server must exist first: the index points at it.
        let (wasm_ep, _) = serve(vec![raw(wasm)]);
        let index = INDEX
            .replace(
                "https://example.com/word-count.wasm",
                &format!("{}/w.wasm", wasm_ep.base),
            )
            .replace(
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
                &sha256_hex(wasm),
            );
        let (index_ep, _) = serve(vec![raw(index.as_bytes())]);
        let entries = fetch_index(&format!("{}/index.toml", index_ep.base)).unwrap();
        assert_eq!(entries.len(), 1);
        let bytes = download_wasm(&entries[0].download_url).unwrap();
        assert_eq!(bytes, wasm);
        verify(&entries[0], &bytes).unwrap();
    }

    /// A minimal `200 OK` with `bytes` as the body.
    fn raw(bytes: &[u8]) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            bytes.len(),
            String::from_utf8_lossy(bytes)
        )
    }
}
