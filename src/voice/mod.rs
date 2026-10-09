//! Voice notes (decision 52): recording the microphone into
//! `assets/voice-<date>-<time>.wav` with a recorder program found on the
//! system (no audio crate: PipeWire's `pw-record`, PulseAudio's
//! `parecord`, ALSA's `arecord` or `ffmpeg`, or a command from
//! `config.toml`), checking and repairing the WAV afterwards, and
//! transcribing it locally with a whisper.cpp binary (`whisper.rs`).
//! Programs are always run with an argument list, never through a shell.
//! No GPUI here.

pub mod private;
pub mod run;
pub mod whisper;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Recordings stop by themselves after this long.
pub const MAX_LENGTH: Duration = Duration::from_secs(30 * 60);
/// 16 kHz mono 16-bit PCM: what whisper.cpp reads.
#[cfg(test)]
pub const RATE: u32 = 16_000;

/// The block a voice note is stored as (an image-style reference, like
/// pasted images: relative to the page's folder).
pub fn note_markdown(file: &str) -> String {
    format!("![voice note](../assets/{file})")
}

/// Whether an `![…](target)` points at audio (shown as a voice note, not
/// an image; left out of exports and published pages).
pub fn is_audio_target(target: &str) -> bool {
    let lower = target.to_ascii_lowercase();
    [
        ".wav", ".mp3", ".ogg", ".oga", ".opus", ".m4a", ".flac", ".webm",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

/// The recorder programs we know, in the order they are looked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    PwRecord,
    Parecord,
    Arecord,
    Ffmpeg,
    /// `voice_recorder` in `config.toml`.
    Custom,
}

impl Kind {
    pub const SEARCH: [Kind; 4] = [Kind::PwRecord, Kind::Parecord, Kind::Arecord, Kind::Ffmpeg];

    pub fn program(self) -> &'static str {
        match self {
            Kind::PwRecord => "pw-record",
            Kind::Parecord => "parecord",
            Kind::Arecord => "arecord",
            Kind::Ffmpeg => "ffmpeg",
            Kind::Custom => "custom recorder",
        }
    }
}

/// A recorder ready to run.
#[derive(Clone, Debug, PartialEq)]
pub struct Recorder {
    pub kind: Kind,
    pub program: PathBuf,
    /// For `Custom`: the arguments, with `{file}` for the output file.
    pub custom_args: Vec<String>,
}

/// What to install when no recorder is found.
pub const NO_RECORDER: &str = "No recorder found: install pipewire (pw-record), libpulse (parecord), alsa-utils (arecord) or ffmpeg, or set voice_recorder in config.toml";

/// `name` as an executable file in one of `path_var`'s folders, or itself
/// if it is a path (contains `/`) to an executable file.
pub fn find_program(name: &str, path_var: Option<&OsStr>) -> Option<PathBuf> {
    if name.contains('/') {
        let path = PathBuf::from(name);
        return is_executable(&path).then_some(path);
    }
    std::env::split_paths(path_var?)
        .map(|dir| dir.join(name))
        .find(|p| is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The recorder to use: the custom command if one is set (and its program
/// exists), else the first of `Kind::SEARCH` on `path_var`.
pub fn detect(custom: &[String], path_var: Option<&OsStr>) -> Result<Recorder, String> {
    if let Some((program, args)) = custom.split_first() {
        let found = find_program(program, path_var).ok_or_else(|| {
            format!("The recorder in config.toml (voice_recorder) was not found: {program}")
        })?;
        return Ok(Recorder {
            kind: Kind::Custom,
            program: found,
            custom_args: args.to_vec(),
        });
    }
    Kind::SEARCH
        .iter()
        .find_map(|&kind| {
            find_program(kind.program(), path_var).map(|program| Recorder {
                kind,
                program,
                custom_args: Vec::new(),
            })
        })
        .ok_or_else(|| NO_RECORDER.to_string())
}

impl Recorder {
    /// The arguments that record 16 kHz mono 16-bit WAV into `out`.
    pub fn args(&self, out: &Path) -> Vec<OsString> {
        let out = out.as_os_str().to_os_string();
        let words = |w: &[&str]| w.iter().map(OsString::from).collect::<Vec<_>>();
        let mut args = match self.kind {
            Kind::PwRecord => words(&["--rate", "16000", "--channels", "1", "--format", "s16"]),
            Kind::Parecord => words(&[
                "--rate=16000",
                "--channels=1",
                "--format=s16le",
                "--file-format=wav",
            ]),
            Kind::Arecord => words(&["-q", "-f", "S16_LE", "-r", "16000", "-c", "1", "-t", "wav"]),
            Kind::Ffmpeg => words(&[
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-f",
                "pulse",
                "-i",
                "default",
                "-ac",
                "1",
                "-ar",
                "16000",
                "-c:a",
                "pcm_s16le",
                "-t",
                "1800",
                "-y",
            ]),
            Kind::Custom => {
                let mut has_file = false;
                let mut args: Vec<OsString> = self
                    .custom_args
                    .iter()
                    .map(|a| {
                        if a.contains("{file}") {
                            has_file = true;
                            let mut s = OsString::new();
                            let mut parts = a.split("{file}");
                            s.push(parts.next().unwrap_or(""));
                            for part in parts {
                                s.push(&out);
                                s.push(part);
                            }
                            s
                        } else {
                            OsString::from(a)
                        }
                    })
                    .collect();
                if !has_file {
                    args.push(out);
                }
                return args;
            }
        };
        args.push(out);
        args
    }

    /// For Settings and statuses: "pw-record (/usr/bin/pw-record)".
    pub fn describe(&self) -> String {
        format!("{} ({})", self.kind.program(), self.program.display())
    }
}

/// `voice-2026-10-09-141503.wav`, or `-2`, `-3`… if taken in `assets`.
pub fn new_file_name(assets: &Path, stamp: &str) -> String {
    let mut n = 1;
    loop {
        let name = match n {
            1 => format!("voice-{stamp}.wav"),
            n => format!("voice-{stamp}-{n}.wav"),
        };
        if !assets.join(&name).exists() {
            return name;
        }
        n += 1;
    }
}

/// "0:07", "12:30", "1:02:03".
pub fn clock(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// A WAV file's format, as read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WavInfo {
    pub format: u16,
    pub channels: u16,
    pub rate: u32,
    pub bits: u16,
    pub data_bytes: usize,
}

impl WavInfo {
    pub fn duration(&self) -> Duration {
        let per_second = u64::from(self.rate) * u64::from(self.channels) * u64::from(self.bits / 8);
        if per_second == 0 {
            return Duration::ZERO;
        }
        Duration::from_millis(self.data_bytes as u64 * 1000 / per_second)
    }
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// `bytes` as a canonical WAV: `RIFF`/`WAVE` with only its `fmt ` and
/// `data` chunks (metadata chunks like `LIST`/`INFO` are dropped) and
/// correct sizes. A recorder stopped before it wrote its header's sizes
/// leaves them 0 or 0xFFFFFFFF: the data then runs to the end of the
/// file. The data is cut to whole sample frames. Errors: not a WAV, no
/// `fmt ` before `data`, not PCM, no audio.
pub fn normalize_wav(bytes: &[u8]) -> Result<(Vec<u8>, WavInfo), String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a WAV file".into());
    }
    let mut pos = 12;
    let mut fmt: Option<&[u8]> = None;
    let data: &[u8] = loop {
        if pos + 8 > bytes.len() {
            return Err("the WAV file has no audio data".into());
        }
        let id = &bytes[pos..pos + 4];
        let size = u32_at(bytes, pos + 4) as usize;
        let body = pos + 8;
        if id == b"data" {
            if fmt.is_none() {
                return Err("the WAV file has no format before its data".into());
            }
            let rest = bytes.len() - body;
            let size = if size == 0 || size == u32::MAX as usize || size > rest {
                rest
            } else {
                size
            };
            break &bytes[body..body + size];
        }
        if body + size > bytes.len() {
            return Err("the WAV file is cut off".into());
        }
        if id == b"fmt " {
            if size < 16 {
                return Err("the WAV format chunk is too short".into());
            }
            fmt = Some(&bytes[body..body + size]);
        }
        pos = body + size + (size & 1);
    };
    let fmt = fmt.unwrap_or_default();
    let info_format = u16_at(fmt, 0);
    if info_format != 1 && info_format != 0xFFFE {
        return Err(format!("the WAV file isn't PCM (format {info_format})"));
    }
    let channels = u16_at(fmt, 2);
    let rate = u32_at(fmt, 4);
    let bits = u16_at(fmt, 14);
    let frame = usize::from(channels) * usize::from(bits / 8);
    if frame == 0 || rate == 0 {
        return Err("the WAV format is invalid".into());
    }
    let data = &data[..data.len() - data.len() % frame];
    if data.is_empty() {
        return Err("no audio was recorded".into());
    }
    let mut out = Vec::with_capacity(44 + data.len());
    let riff_size = 4 + 8 + fmt.len() + (fmt.len() & 1) + 8 + data.len() + (data.len() & 1);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(riff_size as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    out.extend_from_slice(fmt);
    if fmt.len() & 1 == 1 {
        out.push(0);
    }
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    if data.len() & 1 == 1 {
        out.push(0);
    }
    let info = WavInfo {
        format: info_format,
        channels,
        rate,
        bits,
        data_bytes: data.len(),
    };
    Ok((out, info))
}

/// Check and repair the WAV at `path` in place (only rewritten if it
/// changes; atomically).
pub fn finish_wav(path: &Path) -> Result<WavInfo, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("could not read the recording: {e}"))?;
    let (fixed, info) = normalize_wav(&bytes)?;
    if fixed != bytes {
        let tmp = hidden_tmp(path);
        std::fs::write(&tmp, &fixed)
            .and_then(|_| std::fs::rename(&tmp, path))
            .map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("could not repair the recording: {e}")
            })?;
    }
    Ok(info)
}

/// `dir/.name.tmp` for `dir/name`: a name git backup ignores (`.*.tmp`).
fn hidden_tmp(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.tmp"))
}

/// Move a finished recording into the vault as `dest` and remove `src`.
/// A rename when both are on one file system; across file systems (a
/// tmpfs runtime folder to the home disk: EXDEV) a copy instead. Either
/// way the file ends up with the mode a new file in `assets/` gets (as
/// pasted images do), not the private one it was recorded with.
pub fn store(src: &Path, dest: &Path) -> std::io::Result<()> {
    store_with(src, dest, |a, b| std::fs::rename(a, b))
}

/// `store` with the rename to try first (tests make it fail).
pub fn store_with(
    src: &Path,
    dest: &Path,
    rename: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    if !src.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{} is gone", src.display()),
        ));
    }
    let mode = new_file_mode(dest.parent().unwrap_or(Path::new(".")))?;
    match rename(src, dest) {
        Ok(()) => {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dest, std::fs::Permissions::from_mode(mode))
        }
        Err(_) => copy_across(src, dest),
    }
}

/// The permission bits a file created in `dir` gets (0666 less the
/// umask), found by creating one: there's no umask call without libc.
fn new_file_mode(dir: &Path) -> std::io::Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    let probe = dir.join(format!(".mode-{}.tmp", uuid::Uuid::new_v4().simple()));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    let mode = file.metadata().map(|m| m.permissions().mode() & 0o777);
    drop(file);
    let _ = std::fs::remove_file(&probe);
    mode
}

/// `src` copied into a new file next to `dest` (hidden, a name git backup
/// ignores), flushed to disk, renamed to `dest`; then `src` removed.
/// The new file has the normal mode for its folder (not `src`'s, which
/// `fs::copy` would copy).
pub fn copy_across(src: &Path, dest: &Path) -> std::io::Result<()> {
    let tmp = hidden_tmp(dest);
    let copied = (|| {
        let mut from = std::fs::File::open(src)?;
        let mut to = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        std::io::copy(&mut from, &mut to)?;
        to.sync_all()?;
        std::fs::rename(&tmp, dest)
    })();
    if copied.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    copied?;
    if let Some(dir) = dest.parent() {
        // The rename itself on disk (best effort).
        let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
    }
    std::fs::remove_file(src)
}

/// A canonical 16 kHz mono 16-bit WAV of `samples` (tests and stubs).
#[cfg(test)]
pub fn wav_bytes(samples: &[i16]) -> Vec<u8> {
    let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&RATE.to_le_bytes());
    fmt.extend_from_slice(&(RATE * 2).to_le_bytes());
    fmt.extend_from_slice(&2u16.to_le_bytes());
    fmt.extend_from_slice(&16u16.to_le_bytes());
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((4 + 8 + 16 + 8 + data.len()) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&fmt);
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    out
}

#[cfg(test)]
mod tests;
