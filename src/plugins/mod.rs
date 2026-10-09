//! WASM plugins (decision 55, docs/PLUGINS.md): `<graph>/plugins/<id>/`
//! holds `plugin.toml` (the manifest) and `plugin.wasm`. Plugins run in a
//! wasmi sandbox (`sandbox.rs`) with no imports but `env.host_log`, and
//! talk to the app in small TOML documents (`protocol.rs`) whose actions
//! the app checks before applying. No GPUI here.

pub mod marketplace;
pub mod protocol;
pub mod sandbox;
#[cfg(test)]
mod tests;

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The plugin API this NoteSec speaks.
pub const API_VERSION: u32 = 1;
/// The biggest `plugin.wasm` loaded.
pub const MAX_WASM: u64 = 4 << 20;
const MAX_MANIFEST: u64 = 64 << 10;
const MAX_COMMANDS: usize = 16;
const MAX_LABEL: usize = 80;
/// Failures in a row after which a plugin is turned off.
pub const MAX_FAILURES: u32 = 3;

#[derive(Clone, Debug, PartialEq)]
pub struct PluginCommand {
    pub id: String,
    pub label: String,
}

/// A plugin found in `plugins/`, with a valid manifest and binary.
#[derive(Clone, Debug, PartialEq)]
pub struct Plugin {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub commands: Vec<PluginCommand>,
    /// The `{{macro}}` its render hook claims.
    pub render: Option<String>,
    pub wasm: PathBuf,
    /// Hash of `plugin.wasm` (`wasm_hash`): enabling pins it.
    pub hash: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    id: String,
    name: String,
    version: String,
    api_version: u32,
    #[serde(default)]
    description: String,
    #[serde(default)]
    commands: Vec<RawCommand>,
    render: Option<RawRender>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommand {
    id: String,
    label: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRender {
    #[serde(rename = "macro")]
    name: String,
}

/// Plugin, command and macro ids: `a-z`, `0-9` and `-`, starting with a
/// letter or digit, at most 48 characters (so never a path).
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 48
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !id.starts_with('-')
}

/// A hash of a plugin binary: Argon2id (which the vault already uses,
/// decision 54) at its lowest cost over the bytes is an unkeyed,
/// collision-resistant BLAKE2b-based digest; hex.
pub fn wasm_hash(bytes: &[u8]) -> String {
    use argon2::{Algorithm, Argon2, Params, Version};
    let mut out = [0u8; 32];
    let params = Params::new(8, 1, 1, Some(32)).expect("valid params");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(bytes, b"notesec-plugin-v1", &mut out)
        .expect("hashing never fails for these sizes");
    out.iter().map(|b| format!("{b:02x}")).collect()
}

/// Read and check `dir` (a folder named after the plugin's id).
pub fn load(dir: &Path) -> Result<Plugin, String> {
    let folder = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if !valid_id(folder) {
        return Err("folder name isn't a valid plugin id (a-z, 0-9, -)".into());
    }
    let file = |name: &str, max: u64| -> Result<Vec<u8>, String> {
        let path = dir.join(name);
        let meta = fs::symlink_metadata(&path).map_err(|_| format!("no {name}"))?;
        if !meta.file_type().is_file() {
            return Err(format!("{name} isn't a regular file"));
        }
        if meta.len() > max {
            return Err(format!(
                "{name} is too big ({} bytes, at most {max})",
                meta.len()
            ));
        }
        fs::read(&path).map_err(|err| format!("can't read {name}: {err}"))
    };
    let text = file("plugin.toml", MAX_MANIFEST)?;
    let text = String::from_utf8(text).map_err(|_| "plugin.toml isn't UTF-8".to_string())?;
    let raw: RawManifest =
        toml::from_str(&text).map_err(|err| format!("plugin.toml: {}", err.message()))?;
    if raw.id != folder {
        return Err(format!(
            "its id \u{201c}{}\u{201d} isn't its folder's name",
            raw.id
        ));
    }
    if raw.api_version != API_VERSION {
        return Err(format!(
            "it needs plugin API {}; this NoteSec has API {API_VERSION}",
            raw.api_version
        ));
    }
    let short = |s: &str| {
        !s.trim().is_empty() && s.chars().count() <= MAX_LABEL && !s.contains(['\n', '\r'])
    };
    if !short(&raw.name) || !short(&raw.version) || raw.description.chars().count() > 500 {
        return Err("name, version or description is empty or too long".into());
    }
    if raw.commands.len() > MAX_COMMANDS {
        return Err(format!("more than {MAX_COMMANDS} commands"));
    }
    let mut commands = Vec::new();
    for c in raw.commands {
        if !valid_id(&c.id)
            || !short(&c.label)
            || commands.iter().any(|o: &PluginCommand| o.id == c.id)
        {
            return Err(format!(
                "command \u{201c}{}\u{201d}: bad or repeated id, or bad label",
                c.id
            ));
        }
        commands.push(PluginCommand {
            id: c.id,
            label: c.label.trim().to_string(),
        });
    }
    let render = match raw.render {
        Some(r) if valid_id(&r.name) => Some(r.name),
        Some(r) => {
            return Err(format!(
                "render macro \u{201c}{}\u{201d} isn't a valid id",
                r.name
            ))
        }
        None => None,
    };
    let wasm = file("plugin.wasm", MAX_WASM)?;
    Ok(Plugin {
        id: raw.id,
        name: raw.name.trim().to_string(),
        version: raw.version.trim().to_string(),
        description: raw.description.trim().to_string(),
        commands,
        render,
        wasm: dir.join("plugin.wasm"),
        hash: wasm_hash(&wasm),
    })
}

/// Every plugin under `<root>/plugins`, sorted by id, and the folders
/// that aren't valid plugins with why.
pub fn discover(root: &Path) -> (Vec<Plugin>, Vec<(String, String)>) {
    let (mut found, mut errors) = (Vec::new(), Vec::new());
    let Ok(items) = fs::read_dir(root.join("plugins")) else {
        return (found, errors);
    };
    for item in items.flatten() {
        let name = item.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        match item.file_type() {
            Ok(t) if t.is_dir() => match load(&item.path()) {
                Ok(p) => found.push(p),
                Err(err) => errors.push((name, err)),
            },
            _ => errors.push((name, "not a folder".into())),
        }
    }
    found.sort_by(|a, b| a.id.cmp(&b.id));
    errors.sort();
    (found, errors)
}

/// Every `{{name args}}` (or `{{name}}`) in `text`: the args, trimmed.
pub fn macro_calls(text: &str, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        let inner = after[..end].trim();
        let (head, args) = inner.split_once(char::is_whitespace).unwrap_or((inner, ""));
        if head == name {
            out.push(args.trim().to_string());
        }
        rest = &after[end + 2..];
    }
    out
}
