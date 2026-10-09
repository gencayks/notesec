//! Local transcription with a whisper.cpp program (`whisper-cli`, or the
//! older `main`): `-m <model> -f <wav> -l <language> -nt -otxt -of <tmp>`,
//! the text read from `<tmp>.txt` (stdout if there is none). Only this
//! program runs, on this computer; nothing is sent anywhere by NoteSec.
//! What a binary the user chose does is up to that binary.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::private::PrivateDir;
use super::run;

#[derive(Clone, Debug, PartialEq)]
pub struct Whisper {
    pub binary: PathBuf,
    pub model: PathBuf,
    /// `auto`, or a language code like `en`, `de`.
    pub language: String,
}

/// A language code safe to pass on (`auto` otherwise).
pub fn language(code: &str) -> String {
    let code = code.trim().to_ascii_lowercase();
    if (2..=3).contains(&code.len()) && code.chars().all(|c| c.is_ascii_lowercase()) {
        code
    } else {
        "auto".into()
    }
}

/// How long a transcription may take: two minutes plus six times the
/// recording (a slow CPU and a large model).
pub fn timeout(audio: Duration) -> Duration {
    Duration::from_secs(120) + audio * 6
}

impl Whisper {
    pub fn args(&self, wav: &Path, out_base: &Path) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();
        args.push("-m".into());
        args.push(self.model.clone().into());
        args.push("-f".into());
        args.push(wav.into());
        args.push("-l".into());
        args.push(language(&self.language).into());
        args.push("-nt".into());
        args.push("-otxt".into());
        args.push("-of".into());
        args.push(out_base.into());
        args
    }

    /// The text spoken in `wav`.
    pub fn transcribe(&self, wav: &Path, timeout: Duration) -> Result<String, String> {
        if !self.model.is_file() {
            return Err(format!("Whisper model not found: {}", self.model.display()));
        }
        let scratch =
            PrivateDir::new().map_err(|e| format!("Could not make a private folder: {e}"))?;
        let base = scratch.join("transcript");
        let out = run::run(
            &self.binary,
            &self.args(wav, &base),
            timeout,
            scratch.path(),
        )?;
        if !out.status.success() {
            let code = out
                .status
                .code()
                .map_or("a signal".to_string(), |c| c.to_string());
            return Err(format!(
                "Transcription failed (exit {code}): {}",
                run::tail(&out.stderr)
            ));
        }
        let text = match std::fs::read_to_string(base.with_extension("txt")) {
            Ok(text) => text,
            Err(_) => out.stdout,
        };
        Ok(parse_transcript(&text))
    }
}

/// whisper's text output as one paragraph: timestamps
/// (`[00:00:01.000 --> 00:00:03.000]`) and markers like `[BLANK_AUDIO]`
/// or `[Music]` removed, whitespace collapsed.
pub fn parse_transcript(text: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    for line in text.lines() {
        let mut line = line.trim();
        if line.starts_with('[') && line.contains("-->") {
            line = line.split_once(']').map_or("", |(_, rest)| rest).trim();
        }
        let kept = strip_markers(line);
        words.extend(kept.split_whitespace().map(str::to_string));
    }
    words.join(" ")
}

/// `line` without whisper's annotations: single-bracket groups of
/// letters, `_` and spaces (`[BLANK_AUDIO]`, `[Music]`). Anything else in
/// brackets (`[[links]]`, `[1]`) is kept.
fn strip_markers(line: &str) -> String {
    let mut kept = String::new();
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let close = after.find(']');
        let marker = close.filter(|&c| {
            let inner = &after[..c];
            !kept.ends_with('[')
                && !rest[..open].ends_with('[')
                && !after[c + 1..].starts_with(']')
                && !inner.trim().is_empty()
                && inner
                    .chars()
                    .all(|ch| ch.is_alphabetic() || ch == '_' || ch == ' ')
        });
        match marker {
            Some(c) => {
                kept.push_str(&rest[..open]);
                rest = &after[c + 1..];
            }
            None => {
                kept.push_str(&rest[..open + 1]);
                rest = after;
            }
        }
    }
    kept.push_str(rest);
    kept
}

/// Settings' "Test": the program runs (`--help`) and the model looks like
/// a whisper.cpp model. Returns a one-line summary.
pub fn check(binary: &Path, model: &Path) -> Result<String, String> {
    if binary.as_os_str().is_empty() {
        return Err("No whisper program set".into());
    }
    if super::find_program(
        &binary.to_string_lossy(),
        std::env::var_os("PATH").as_deref(),
    )
    .is_none()
    {
        return Err(format!("Not an executable file: {}", binary.display()));
    }
    let scratch = PrivateDir::new().map_err(|e| format!("Could not make a private folder: {e}"))?;
    let out = run::run(
        binary,
        &["--help".into()],
        Duration::from_secs(10),
        scratch.path(),
    )?;
    let help = format!("{}{}", out.stdout, out.stderr).to_lowercase();
    if !out.status.success() && !help.contains("usage") {
        return Err(format!(
            "{} --help failed: {}",
            binary.display(),
            run::tail(&format!("{}\n{}", out.stdout, out.stderr))
        ));
    }
    if model.as_os_str().is_empty() {
        return Err("The program runs, but no model is set".into());
    }
    let mut magic = [0u8; 4];
    let read =
        std::fs::File::open(model).and_then(|mut f| std::io::Read::read_exact(&mut f, &mut magic));
    if read.is_err() {
        return Err(format!(
            "Model not found or unreadable: {}",
            model.display()
        ));
    }
    if !matches!(&magic, b"lmgg" | b"ggml" | b"GGUF" | b"ggjt") {
        return Err(format!(
            "Not a whisper.cpp model (ggml): {}",
            model.display()
        ));
    }
    let size = std::fs::metadata(model).map(|m| m.len()).unwrap_or(0);
    Ok(format!(
        "Ready: {} with {} ({} MB)",
        binary.file_name().unwrap_or_default().to_string_lossy(),
        model.file_name().unwrap_or_default().to_string_lossy(),
        size / 1_000_000
    ))
}
