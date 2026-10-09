use super::archive::*;
use super::*;
use std::path::PathBuf;

const PASS: &str = "correct horse battery";

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("notesec-vault-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A graph with notes, secrets and things that must stay out.
fn graph(name: &str) -> PathBuf {
    let g = temp(name);
    for (path, text) in [
        ("pages/A.md", "- alpha [[B]]\n"),
        ("pages/Board.md", "- type:: whiteboard\n- card\n  x:: 1\n  y:: 2\n"),
        ("journals/2026_10_09.md", "- today\n"),
        ("assets/pic.png", "PNGDATA"),
        ("assets/sub/voice.wav", "RIFF"),
        ("config.toml", "theme = \"dark\"\n"),
        (
            "state.toml",
            "favorites = [\"A\"]\nclipper_token = \"tok-SECRET-1\"\nai_api_key = \"sk-SECRET-2\"\n\n[nested]\nai_api_key = \"sk-SECRET-3\"\n",
        ),
        (".git/config", "[core]\n"),
        (".trash/old/pages/X.md", "- deleted\n"),
        (".notesec/embeddings.bin", "cache"),
        ("exports/A.html", "<html>"),
        ("published/index.html", "<html>"),
        ("pages/.A.md.tmp", "half-saved"),
        ("notes.txt", "stray"),
    ] {
        let p = g.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }
    std::os::unix::fs::symlink("/etc/passwd", g.join("pages/link.md")).unwrap();
    std::os::unix::fs::symlink("/etc", g.join("assets/etc")).unwrap();
    g
}

fn paths(entries: &[Entry]) -> Vec<&str> {
    entries.iter().map(|e| e.path.as_str()).collect()
}

#[test]
fn only_notes_and_settings_without_secrets_go_in() {
    let g = graph("collect");
    let entries = collect(&g).unwrap();
    assert_eq!(
        paths(&entries),
        [
            "assets/pic.png",
            "assets/sub/voice.wav",
            "config.toml",
            "journals/2026_10_09.md",
            "pages/A.md",
            "pages/Board.md",
            "state.toml"
        ]
    );
    let plain = pack(&entries).unwrap();
    let text = String::from_utf8_lossy(&plain);
    for secret in [
        "SECRET",
        "clipper_token",
        "ai_api_key",
        "deleted",
        "cache",
        "half-saved",
        "root:",
    ] {
        assert!(!text.contains(secret), "{secret} leaked");
    }
    let state = entries.iter().find(|e| e.path == "state.toml").unwrap();
    let state: toml::Table = std::str::from_utf8(&state.bytes).unwrap().parse().unwrap();
    assert_eq!(
        state["favorites"].as_array().unwrap().len(),
        1,
        "the rest is kept"
    );
    // An unreadable state.toml is dropped whole rather than risk a secret.
    assert!(strip_secrets(b"clipper_token = \"x\" = broken").is_empty());
    let _ = fs::remove_dir_all(g);
}

#[test]
fn a_vault_round_trips_into_a_new_folder() {
    let g = graph("round");
    let out = temp("round-out");
    let file = out.join("notes.notesec-vault");
    assert_eq!(export(&g, &file, PASS, TEST_KDF).unwrap(), 7);
    let bytes = fs::read(&file).unwrap();
    assert!(bytes.starts_with(MAGIC));
    assert!(
        !String::from_utf8_lossy(&bytes).contains("alpha"),
        "encrypted"
    );
    assert!(!out.read_dir().unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .ends_with(".tmp")));

    let dest = temp("round-dest");
    assert_eq!(import(&file, &dest, PASS).unwrap(), 7);
    for path in [
        "pages/A.md",
        "pages/Board.md",
        "journals/2026_10_09.md",
        "assets/sub/voice.wav",
        "config.toml",
    ] {
        assert_eq!(
            fs::read(dest.join(path)).unwrap(),
            fs::read(g.join(path)).unwrap(),
            "{path}"
        );
    }
    let state = fs::read_to_string(dest.join("state.toml")).unwrap();
    assert!(state.contains("favorites") && !state.contains("SECRET"));
    for absent in [
        ".git",
        ".trash",
        ".notesec",
        "exports",
        "published",
        "notes.txt",
        "pages/link.md",
        "assets/etc",
    ] {
        assert!(!dest.join(absent).exists(), "{absent}");
    }
    let names: Vec<_> = dest
        .read_dir()
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 5, "no staging folder left: {names:?}");

    // Never into a folder with something in it.
    let err = import(&file, &dest, PASS).unwrap_err();
    assert!(err.contains("isn't empty"), "{err}");
    for d in [g, out, dest] {
        let _ = fs::remove_dir_all(d);
    }
}

fn sealed(plain: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    seal(plain, PASS, TEST_KDF, &mut out).unwrap();
    out
}

#[test]
fn wrong_passphrase_and_tampering_fail() {
    // Three chunks: two full, one short.
    let plain: Vec<u8> = (0..2 * CHUNK as usize + 100)
        .map(|i| (i % 251) as u8)
        .collect();
    let file = sealed(&plain);
    assert_eq!(open(&file[..], PASS).unwrap(), plain);
    let wrong = open(&file[..], "correct horse battery!").unwrap_err();
    assert_eq!(wrong, WRONG);

    let flip = |at: usize| {
        let mut f = file.clone();
        f[at] ^= 1;
        open(&f[..], PASS)
    };
    // The salt and the nonce are authenticated (AAD), the chunks too.
    assert!(flip(HEADER_LEN - 1).is_err(), "nonce");
    assert!(flip(30).is_err(), "salt");
    assert!(flip(HEADER_LEN + 5).is_err(), "first chunk");
    assert!(flip(file.len() - 1).is_err(), "last tag");
    // A changed KDF setting still in bounds: wrong key, refused.
    let mut f = file.clone();
    f[21] ^= 1; // t: 1 -> 0 is out of bounds; p byte below
    assert!(open(&f[..], PASS).is_err());

    // Truncation: at a chunk boundary, inside a chunk, the header alone.
    let one = HEADER_LEN + CHUNK as usize + TAG_LEN;
    for cut in [
        one,
        2 * one - HEADER_LEN,
        one + 10,
        file.len() - 1,
        HEADER_LEN,
        10,
    ] {
        assert!(open(&file[..cut], PASS).is_err(), "cut at {cut}");
    }
    // An extra chunk appended, or two chunks swapped.
    let mut longer = file.clone();
    longer.extend_from_slice(&file[HEADER_LEN..HEADER_LEN + 40]);
    assert!(open(&longer[..], PASS).is_err());
    let mut swapped = file.clone();
    let (a, b) = (HEADER_LEN..one, one..one + (one - HEADER_LEN));
    let (ca, cb) = (file[a.clone()].to_vec(), file[b.clone()].to_vec());
    swapped[a].copy_from_slice(&cb);
    swapped[b].copy_from_slice(&ca);
    assert!(open(&swapped[..], PASS).is_err());

    // Not a vault at all; an empty vault is fine.
    assert_eq!(
        open(&b"hello"[..], PASS).unwrap_err(),
        "Not a NoteSec vault file"
    );
    assert_eq!(open(&sealed(&[])[..], PASS).unwrap(), Vec::<u8>::new());
}

#[test]
fn a_failed_import_writes_nothing() {
    let g = graph("fail");
    let out = temp("fail-out");
    let file = out.join("v.notesec-vault");
    export(&g, &file, PASS, TEST_KDF).unwrap();
    let dest = temp("fail-dest");
    assert!(import(&file, &dest, "not the passphrase").is_err());
    let mut bytes = fs::read(&file).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x80;
    fs::write(&file, &bytes).unwrap();
    assert_eq!(import(&file, &dest, PASS).unwrap_err(), WRONG);
    assert_eq!(dest.read_dir().unwrap().count(), 0);
    for d in [g, out, dest] {
        let _ = fs::remove_dir_all(d);
    }
}

#[test]
fn kdf_settings_are_bounded() {
    assert!(DEFAULT_KDF.check().is_ok() && TEST_KDF.check().is_ok());
    for bad in [
        Kdf {
            m_kib: 4 << 20,
            t: 3,
            p: 1,
        },
        Kdf {
            m_kib: 64,
            t: 0,
            p: 1,
        },
        Kdf {
            m_kib: 64,
            t: 100,
            p: 1,
        },
        Kdf {
            m_kib: 64,
            t: 1,
            p: 64,
        },
        Kdf {
            m_kib: 4,
            t: 1,
            p: 1,
        },
    ] {
        assert!(bad.check().is_err(), "{bad:?}");
    }
    // A header asking for 4 GiB is refused before any key derivation.
    let mut file = sealed(b"x");
    file[10..14].copy_from_slice(&(4u32 << 20).to_be_bytes());
    let err = open(&file[..], PASS).unwrap_err();
    assert!(err.contains("unreasonable"), "{err}");
    let mut file = sealed(b"x");
    file[8..10].copy_from_slice(&9u16.to_be_bytes());
    assert!(open(&file[..], PASS).unwrap_err().contains("version 9"));
}

fn raw(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = ARCHIVE_MAGIC.to_vec();
    for (path, bytes) in entries {
        out.extend_from_slice(&(path.len() as u32).to_be_bytes());
        out.extend_from_slice(path.as_bytes());
        out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        out.extend_from_slice(bytes);
    }
    out.extend_from_slice(&0u32.to_be_bytes());
    out
}

#[test]
fn unsafe_archive_paths_are_refused() {
    assert_eq!(unpack(&raw(&[("pages/A.md", b"- a\n")])).unwrap().len(), 1);
    for path in [
        "../evil",
        "pages/../../evil",
        "/etc/passwd",
        "pages//A.md",
        "pages/./A.md",
        "pages\\..\\x",
        "C:/x",
        "src/main.rs",
        ".git/config",
        "notes.txt",
        "pages/a\0b",
        "",
    ] {
        let err = unpack(&raw(&[(path, b"x")]));
        assert!(err.is_err(), "{path:?} accepted");
    }
    // Duplicates (also by case) and a file where a folder is.
    assert!(unpack(&raw(&[("pages/A.md", b"1"), ("pages/a.md", b"2")])).is_err());
    assert!(unpack(&raw(&[("assets/x", b"1"), ("assets/x/y", b"2")])).is_err());
    assert!(unpack(&raw(&[("assets/x/y", b"1"), ("assets/x", b"2")])).is_err());
    // Lengths past the end, trailing bytes, a bad magic.
    let mut short = raw(&[("pages/A.md", b"abc")]);
    short.truncate(short.len() - 6);
    assert!(unpack(&short).is_err());
    let mut trailing = raw(&[("pages/A.md", b"abc")]);
    trailing.push(0);
    assert!(unpack(&trailing).is_err());
    assert!(unpack(b"NOTANARC").is_err());
    // A vault carrying a traversal entry writes nothing.
    let mut file = Vec::new();
    seal(
        &raw(&[("pages/A.md", b"ok"), ("../escape", b"x")]),
        PASS,
        TEST_KDF,
        &mut file,
    )
    .unwrap();
    let dir = temp("traversal");
    let dest = dir.join("dest");
    fs::create_dir(&dest).unwrap();
    fs::write(dir.join("v.notesec-vault"), &file).unwrap();
    assert!(import(&dir.join("v.notesec-vault"), &dest, PASS)
        .unwrap_err()
        .contains("../escape"));
    assert_eq!(dest.read_dir().unwrap().count(), 0);
    assert!(!dir.join("escape").exists());
    let _ = fs::remove_dir_all(dir);
}
