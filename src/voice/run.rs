//! Running the recorder and whisper: argument lists only (no shell),
//! output to temporary files (a full pipe can't stall a long run),
//! timeouts, and stopping with SIGINT through the `kill` program (so the
//! recorder writes its header's sizes; no `libc`, no `unsafe`).

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Start `program args`, no input, output into `log`.
pub fn spawn(program: &Path, args: &[OsString], log: &Path) -> std::io::Result<Child> {
    let out = File::create(log)?;
    let err = out.try_clone()?;
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
}

/// Wait up to `timeout` for `child` to end.
pub fn wait_for(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let end = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(20)),
            _ => return None,
        }
    }
}

/// Ask `child` to finish (SIGINT, as Ctrl+C would: recorders then close
/// their file properly), waiting up to `grace`; then kill it. Returns
/// whether it ended on the interrupt.
pub fn interrupt(child: &mut Child, path_var: Option<&OsStr>, grace: Duration) -> bool {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return true;
    }
    let kill = super::find_program("kill", path_var).or_else(|| {
        ["/usr/bin/kill", "/bin/kill"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
    });
    let sent = kill.is_some_and(|kill| {
        Command::new(kill)
            .args(["-INT", &child.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    });
    if sent && wait_for(child, grace).is_some() {
        return true;
    }
    let _ = child.kill();
    let _ = child.wait();
    false
}

/// What a finished run printed.
pub struct Output {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

/// Run `program args` to the end, at most `timeout`. Its output is
/// collected in files in `scratch` (a private folder: stdout may be a
/// transcript).
pub fn run(
    program: &Path,
    args: &[OsString],
    timeout: Duration,
    scratch: &Path,
) -> Result<Output, String> {
    let out = scratch.join("stdout.txt");
    let err = scratch.join("stderr.txt");
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(File::create(&out).map_err(|e| e.to_string())?)
        .stderr(File::create(&err).map_err(|e| e.to_string())?)
        .spawn()
        .map_err(|e| format!("could not run {}: {e}", program.display()))?;
    let Some(status) = wait_for(&mut child, timeout) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!(
            "{} took longer than {} s and was stopped",
            program.display(),
            timeout.as_secs()
        ));
    };
    Ok(Output {
        status,
        stdout: read_lossy(&out),
        stderr: read_lossy(&err),
    })
}

/// A file's text, invalid UTF-8 replaced; empty if unreadable.
pub fn read_lossy(path: &Path) -> String {
    std::fs::read(path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// The last few non-empty lines of `text`, at most 300 bytes.
pub fn tail(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let mut tail = lines[lines.len().saturating_sub(3)..].join(" | ");
    if tail.len() > 300 {
        let mut cut = tail.len() - 300;
        while !tail.is_char_boundary(cut) {
            cut += 1;
        }
        tail = format!("\u{2026}{}", &tail[cut..]);
    }
    tail
}
