use super::whisper::{self, Whisper};
use super::*;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;

fn stub(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn tmp() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("notesec-voice-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn strings(args: &[OsString]) -> Vec<String> {
    args.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn detection_follows_the_order_on_a_fake_path() {
    let root = tmp();
    let (a, b) = (root.join("a"), root.join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let path_var = std::env::join_paths([&a, &b]).unwrap();

    assert_eq!(detect(&[], Some(&path_var)), Err(NO_RECORDER.to_string()));
    assert!(NO_RECORDER.contains("pipewire") && NO_RECORDER.contains("alsa-utils"));

    stub(&b, "ffmpeg", "exit 0");
    assert_eq!(detect(&[], Some(&path_var)).unwrap().kind, Kind::Ffmpeg);
    // Not executable: skipped.
    std::fs::write(a.join("arecord"), "x").unwrap();
    assert_eq!(detect(&[], Some(&path_var)).unwrap().kind, Kind::Ffmpeg);
    stub(&b, "arecord", "exit 0");
    let found = detect(&[], Some(&path_var)).unwrap();
    assert_eq!(
        (found.kind, found.program),
        (Kind::Arecord, b.join("arecord"))
    );
    stub(&b, "parecord", "exit 0");
    assert_eq!(detect(&[], Some(&path_var)).unwrap().kind, Kind::Parecord);
    stub(&a, "pw-record", "exit 0");
    assert_eq!(detect(&[], Some(&path_var)).unwrap().kind, Kind::PwRecord);
    // A directory with the name doesn't count.
    assert_eq!(
        find_program("b", Some(&std::env::join_paths([&root]).unwrap())),
        None
    );

    // A custom recorder wins, by name on PATH or by path.
    stub(&b, "myrec", "exit 0");
    let custom = vec!["myrec".to_string(), "--out={file}".to_string()];
    let found = detect(&custom, Some(&path_var)).unwrap();
    assert_eq!(
        (found.kind, found.program.clone()),
        (Kind::Custom, b.join("myrec"))
    );
    let by_path = vec![b.join("myrec").display().to_string()];
    assert_eq!(detect(&by_path, None).unwrap().kind, Kind::Custom);
    let missing = detect(&["nope".to_string()], Some(&path_var)).unwrap_err();
    assert!(
        missing.contains("voice_recorder") && missing.contains("nope"),
        "{missing}"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn command_lines_per_recorder() {
    let out = Path::new("/v/assets/voice-x.wav");
    let rec = |kind, custom: &[&str]| Recorder {
        kind,
        program: PathBuf::from("/bin/x"),
        custom_args: custom.iter().map(|s| s.to_string()).collect(),
    };
    assert_eq!(
        strings(&rec(Kind::PwRecord, &[]).args(out)),
        [
            "--rate",
            "16000",
            "--channels",
            "1",
            "--format",
            "s16",
            "/v/assets/voice-x.wav"
        ]
    );
    assert_eq!(
        strings(&rec(Kind::Parecord, &[]).args(out)),
        [
            "--rate=16000",
            "--channels=1",
            "--format=s16le",
            "--file-format=wav",
            "/v/assets/voice-x.wav"
        ]
    );
    assert_eq!(
        strings(&rec(Kind::Arecord, &[]).args(out)),
        [
            "-q",
            "-f",
            "S16_LE",
            "-r",
            "16000",
            "-c",
            "1",
            "-t",
            "wav",
            "/v/assets/voice-x.wav"
        ]
    );
    let ff = strings(&rec(Kind::Ffmpeg, &[]).args(out));
    assert_eq!(ff.last().unwrap(), "/v/assets/voice-x.wav");
    for pair in [
        ["-ar", "16000"],
        ["-ac", "1"],
        ["-c:a", "pcm_s16le"],
        ["-f", "pulse"],
    ] {
        assert!(ff.windows(2).any(|w| w == pair), "{pair:?} in {ff:?}");
    }
    // Custom: `{file}` is replaced (also inside an argument), else appended.
    assert_eq!(
        strings(&rec(Kind::Custom, &["-o", "{file}", "--x={file}.y"]).args(out)),
        ["-o", "/v/assets/voice-x.wav", "--x=/v/assets/voice-x.wav.y"]
    );
    assert_eq!(
        strings(&rec(Kind::Custom, &["-q"]).args(out)),
        ["-q", "/v/assets/voice-x.wav"]
    );
    // A file name with spaces or quotes stays one argument: no shell.
    let odd = Path::new("/v/my notes/a'b; rm -rf x.wav");
    assert_eq!(
        rec(Kind::Arecord, &[]).args(odd).last().unwrap(),
        odd.as_os_str()
    );
}

fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut c = id.to_vec();
    c.extend_from_slice(&(body.len() as u32).to_le_bytes());
    c.extend_from_slice(body);
    if body.len() % 2 == 1 {
        c.push(0);
    }
    c
}

fn fmt16() -> Vec<u8> {
    wav_bytes(&[])[20..36].to_vec()
}

#[test]
fn wav_repair() {
    let good = wav_bytes(&[1, -2, 3, 400]);
    let (same, info) = normalize_wav(&good).unwrap();
    assert_eq!(same, good);
    assert_eq!(
        (info.rate, info.channels, info.bits, info.data_bytes),
        (16000, 1, 16, 8)
    );
    assert_eq!(info.duration(), Duration::ZERO);
    let second = wav_bytes(&vec![0; 16000]);
    assert_eq!(
        normalize_wav(&second).unwrap().1.duration(),
        Duration::from_secs(1)
    );

    // A recorder killed before it wrote its sizes: 0 or 0xFFFFFFFF.
    for bad in [0u32, u32::MAX, 1000] {
        let mut b = good.clone();
        b[4..8].copy_from_slice(&bad.to_le_bytes());
        b[40..44].copy_from_slice(&bad.to_le_bytes());
        assert_eq!(normalize_wav(&b).unwrap().0, good, "size {bad}");
    }
    // A half sample at the end is cut off.
    let mut odd = good.clone();
    odd.push(7);
    odd[40..44].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(normalize_wav(&odd).unwrap().0, good);

    // Metadata chunks (LIST, odd-sized with padding) are dropped.
    let mut with_list = b"RIFF\0\0\0\0WAVE".to_vec();
    with_list.extend(chunk(b"LIST", b"INFOabc"));
    with_list.extend(chunk(b"fmt ", &fmt16()));
    with_list.extend(chunk(b"fact", &[0; 4]));
    with_list.extend(chunk(b"data", &good[44..]));
    with_list.extend(chunk(b"id3 ", b"tail"));
    let (fixed, info) = normalize_wav(&with_list).unwrap();
    assert_eq!(fixed, good);
    assert_eq!(info.data_bytes, 8);

    // An 18-byte fmt (cbSize) is kept as it is.
    let mut fmt18 = fmt16();
    fmt18.extend_from_slice(&[0, 0]);
    let mut b = b"RIFF\0\0\0\0WAVE".to_vec();
    b.extend(chunk(b"fmt ", &fmt18));
    b.extend(chunk(b"data", &[1, 0, 2, 0]));
    let (fixed, _) = normalize_wav(&b).unwrap();
    assert_eq!(
        u32::from_le_bytes(fixed[4..8].try_into().unwrap()) as usize,
        fixed.len() - 8
    );
    assert_eq!(normalize_wav(&fixed).unwrap().0, fixed);

    // Errors.
    assert!(normalize_wav(b"hello").is_err());
    assert!(normalize_wav(&good[..44]).unwrap_err().contains("no audio"));
    assert!(normalize_wav(&good[..36])
        .unwrap_err()
        .contains("no audio data"));
    let mut no_fmt = b"RIFF\0\0\0\0WAVE".to_vec();
    no_fmt.extend(chunk(b"data", &[0; 4]));
    assert!(normalize_wav(&no_fmt).unwrap_err().contains("format"));
    let mut float = good.clone();
    float[20..22].copy_from_slice(&3u16.to_le_bytes());
    assert!(normalize_wav(&float).unwrap_err().contains("PCM"));
    let mut cut = b"RIFF\0\0\0\0WAVE".to_vec();
    cut.extend_from_slice(b"LIST\xff\0\0\0ab");
    assert!(normalize_wav(&cut).unwrap_err().contains("cut off"));

    // finish_wav repairs in place.
    let dir = tmp();
    let file = dir.join("v.wav");
    std::fs::write(&file, &odd).unwrap();
    assert_eq!(finish_wav(&file).unwrap().data_bytes, 8);
    assert_eq!(std::fs::read(&file).unwrap(), good);
    assert!(!dir.join(".v.wav.tmp").exists());
    // store: moved into place, the source gone.
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    store(&file, &dir.join("assets/voice-1.wav")).unwrap();
    assert!(!file.exists());
    assert_eq!(std::fs::read(dir.join("assets/voice-1.wav")).unwrap(), good);
    assert!(store(&file, &dir.join("assets/voice-2.wav")).is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn names_and_clock() {
    let dir = tmp();
    assert_eq!(
        new_file_name(&dir, "2026-10-09-141503"),
        "voice-2026-10-09-141503.wav"
    );
    std::fs::write(dir.join("voice-2026-10-09-141503.wav"), "").unwrap();
    assert_eq!(
        new_file_name(&dir, "2026-10-09-141503"),
        "voice-2026-10-09-141503-2.wav"
    );
    std::fs::remove_dir_all(dir).unwrap();
    assert_eq!(clock(Duration::from_secs(7)), "0:07");
    assert_eq!(clock(Duration::from_secs(750)), "12:30");
    assert_eq!(clock(Duration::from_secs(3723)), "1:02:03");
    assert_eq!(
        note_markdown("voice-a.wav"),
        "![voice note](../assets/voice-a.wav)"
    );
    assert!(is_audio_target("../assets/voice-a.wav") && is_audio_target("x.MP3"));
    assert!(!is_audio_target("../assets/a.png"));
}

#[test]
fn transcript_parsing() {
    let txt = "[00:00:00.000 --> 00:00:02.500]   Hello there.\n\
               [00:00:02.500 --> 00:00:04.000]  [BLANK_AUDIO]\n\
               [00:00:04.000 --> 00:00:06.000]  Second   line [Music] ok\n";
    assert_eq!(
        whisper::parse_transcript(txt),
        "Hello there. Second line ok"
    );
    assert_eq!(
        whisper::parse_transcript(" Plain text\n\n more\n"),
        "Plain text more"
    );
    assert_eq!(whisper::parse_transcript("[BLANK_AUDIO]\n"), "");
    assert_eq!(whisper::parse_transcript("an [unclosed"), "an [unclosed");
    assert_eq!(
        whisper::parse_transcript("see [[a b]] and [1] [ (laughs)"),
        "see [[a b]] and [1] [ (laughs)"
    );
    assert_eq!(whisper::language("DE"), "de");
    assert_eq!(whisper::language(""), "auto");
    assert_eq!(whisper::language("-m x"), "auto");
    assert_eq!(
        whisper::timeout(Duration::from_secs(10)),
        Duration::from_secs(180)
    );
    let w = Whisper {
        binary: "/w/whisper-cli".into(),
        model: "/m/ggml-base.bin".into(),
        language: "auto".into(),
    };
    assert_eq!(
        strings(&w.args(Path::new("/a/v.wav"), Path::new("/t/out"))),
        [
            "-m",
            "/m/ggml-base.bin",
            "-f",
            "/a/v.wav",
            "-l",
            "auto",
            "-nt",
            "-otxt",
            "-of",
            "/t/out"
        ]
    );
}

#[test]
fn transcription_with_a_stub_whisper() {
    let dir = tmp();
    let wav = dir.join("v.wav");
    std::fs::write(&wav, wav_bytes(&[0; 100])).unwrap();
    let model = dir.join("m.bin");
    std::fs::write(&model, b"lmgg....").unwrap();
    // Writes `<-of>.txt` like whisper-cli; checks it got the WAV.
    let good = stub(
        &dir,
        "whisper-ok",
        r#"while [ $# -gt 0 ]; do case "$1" in -of) base="$2"; shift;; -f) wav="$2"; shift;; --help) echo "usage: whisper"; exit 0;; esac; shift; done
[ -f "$wav" ] || exit 3
printf '[00:00:00.000 --> 00:00:01.000]  Buy [[milk]]\n' > "$base.txt""#,
    );
    let w = |binary: &Path| Whisper {
        binary: binary.to_path_buf(),
        model: model.clone(),
        language: "en".into(),
    };
    assert_eq!(
        w(&good).transcribe(&wav, Duration::from_secs(10)).unwrap(),
        "Buy [[milk]]"
    );
    // Only stdout: still read.
    let stdout = stub(&dir, "whisper-stdout", "echo ' spoken words'");
    assert_eq!(
        w(&stdout)
            .transcribe(&wav, Duration::from_secs(10))
            .unwrap(),
        "spoken words"
    );
    // Failure: exit code and the end of stderr.
    let bad = stub(
        &dir,
        "whisper-bad",
        "echo 'loading' >&2; echo 'error: bad model' >&2; exit 2",
    );
    let err = w(&bad)
        .transcribe(&wav, Duration::from_secs(10))
        .unwrap_err();
    assert!(
        err.contains("exit 2") && err.contains("error: bad model"),
        "{err}"
    );
    // Too slow: stopped.
    let slow = stub(&dir, "whisper-slow", "sleep 5");
    let err = w(&slow)
        .transcribe(&wav, Duration::from_millis(200))
        .unwrap_err();
    assert!(err.contains("longer than"), "{err}");
    // Missing model, missing program.
    let mut no_model = w(&good);
    no_model.model = dir.join("none.bin");
    assert!(no_model
        .transcribe(&wav, Duration::from_secs(5))
        .unwrap_err()
        .contains("model not found"));
    assert!(w(&dir.join("nope"))
        .transcribe(&wav, Duration::from_secs(5))
        .unwrap_err()
        .contains("could not run"));

    // Settings' Test.
    assert!(whisper::check(&good, &model)
        .unwrap()
        .starts_with("Ready: whisper-ok with m.bin"));
    let text_model = dir.join("t.bin");
    std::fs::write(&text_model, "hello").unwrap();
    assert!(whisper::check(&good, &text_model)
        .unwrap_err()
        .contains("Not a whisper.cpp model"));
    assert!(whisper::check(&good, &dir.join("x.bin"))
        .unwrap_err()
        .contains("not found"));
    assert!(whisper::check(&dir.join("nope"), &model)
        .unwrap_err()
        .contains("Not an executable"));
    assert!(whisper::check(&bad, &model)
        .unwrap_err()
        .contains("--help failed"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn stopping_a_recorder_with_sigint() {
    let dir = tmp();
    let out = dir.join("rec.wav");
    // Writes a WAV with unset sizes, then finishes it on SIGINT.
    let rec = stub(
        &dir,
        "rec",
        r#"trap 'printf done > "$1.int"; exit 0' INT
printf 'x' > "$1"
while true; do sleep 0.05; done"#,
    );
    let log = dir.join("log");
    let mut child = run::spawn(&rec, &[out.clone().into()], &log).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(run::interrupt(
        &mut child,
        std::env::var_os("PATH").as_deref(),
        Duration::from_secs(5)
    ));
    assert_eq!(
        std::fs::read_to_string(dir.join("rec.wav.int")).unwrap(),
        "done"
    );
    // One that ignores SIGINT is killed after the grace time.
    let stubborn = stub(
        &dir,
        "stubborn",
        "trap '' INT\nwhile true; do sleep 0.05; done",
    );
    let mut child = run::spawn(&stubborn, &[], &log).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(!run::interrupt(
        &mut child,
        None,
        Duration::from_millis(300)
    ));
    assert!(child.try_wait().unwrap().is_some());
    assert_eq!(run::tail("a\n\nb\nc\nd\n"), "b | c | d");
    std::fs::remove_dir_all(dir).unwrap();
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn recordings_go_into_private_folders() {
    use super::private::{runtime_base, PrivateDir};
    let root = tmp();
    // Fresh, random, 0700, removed with what's inside.
    let a = PrivateDir::new_in(&root).unwrap();
    let b = PrivateDir::new_in(&root).unwrap();
    assert_ne!(a.path(), b.path());
    assert_eq!(mode(a.path()), 0o700);
    std::fs::write(a.join("recording.wav"), "x").unwrap();
    let kept = a.path().to_path_buf();
    drop(a);
    assert!(!kept.exists());

    // $XDG_RUNTIME_DIR/notesec-voice: made 0700, reused, refused if a
    // symlink or open to others.
    let runtime = root.join("run");
    std::fs::create_dir(&runtime).unwrap();
    let base = runtime_base(&runtime).unwrap();
    assert_eq!(base, runtime.join("notesec-voice"));
    assert_eq!(mode(&base), 0o700);
    assert_eq!(runtime_base(&runtime).unwrap(), base);
    let inner = PrivateDir::new_in(&base).unwrap();
    assert!(inner.path().starts_with(&base) && mode(inner.path()) == 0o700);
    drop(inner);
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
    let err = runtime_base(&runtime).unwrap_err();
    assert!(err.to_string().contains("open to other users"), "{err}");
    std::fs::remove_dir(&base).unwrap();
    std::os::unix::fs::symlink(&root, &base).unwrap();
    let err = runtime_base(&runtime).unwrap_err();
    assert!(err.to_string().contains("not a folder"), "{err}");
    // Without a usable runtime folder: the temp folder, still 0700.
    let fallback = PrivateDir::new().unwrap();
    assert_eq!(mode(fallback.path()), 0o700);
    drop(fallback);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn storing_across_file_systems_copies_with_the_assets_mode() {
    let root = tmp();
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).unwrap();
    let private = super::private::PrivateDir::new_in(&root).unwrap();
    // What a new file in assets/ gets (as a pasted image does).
    let probe = assets.join("image.png");
    std::fs::write(&probe, "png").unwrap();
    let asset_mode = mode(&probe);
    let wav = wav_bytes(&[5; 64]);

    // A rename that fails like tmpfs → home (EXDEV): the copy fallback.
    let src = private.join("recording.wav");
    std::fs::write(&src, &wav).unwrap();
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o600)).unwrap();
    let dest = assets.join("voice-a.wav");
    let exdev = |_: &Path, _: &Path| Err(std::io::Error::from_raw_os_error(18));
    store_with(&src, &dest, exdev).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), wav);
    assert!(!src.exists());
    assert_eq!(mode(&dest), asset_mode, "not the private 0600");
    // No leftovers (the hidden temporary or the mode probe).
    let names: Vec<String> = std::fs::read_dir(&assets)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");

    // The same file system: a rename, mode set the same way.
    std::fs::write(&src, &wav).unwrap();
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o600)).unwrap();
    let dest = assets.join("voice-b.wav");
    store(&src, &dest).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), wav);
    assert_eq!(mode(&dest), asset_mode);

    // A copy that fails leaves nothing behind and keeps the source.
    std::fs::write(&src, &wav).unwrap();
    let taken = assets.join("voice-b.wav");
    let err = copy_across(&src, &assets.join("missing-dir/x.wav")).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert!(src.exists() && taken.exists());
    assert!(store(&root.join("none.wav"), &assets.join("c.wav")).is_err());

    // A real other file system if this machine has one (/dev/shm).
    let shm = Path::new("/dev/shm");
    if shm.is_dir() {
        if let Ok(other) = super::private::PrivateDir::new_in(shm) {
            let src = other.join("r.wav");
            std::fs::write(&src, &wav).unwrap();
            store(&src, &assets.join("voice-c.wav")).unwrap();
            assert_eq!(std::fs::read(assets.join("voice-c.wav")).unwrap(), wav);
            assert!(!src.exists());
        }
    }
    drop(private);
    std::fs::remove_dir_all(root).unwrap();
}
