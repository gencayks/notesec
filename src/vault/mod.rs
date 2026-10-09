//! Encrypted vault export/import (decision 54, docs/ENCRYPTION.md): the
//! whole graph in one `.notesec-vault` file, for a USB stick or a cloud
//! drive. Not live sync (future work). No GPUI here.
//!
//! File: a `HEADER_LEN`-byte header (magic, version, Argon2id m/t/p, chunk
//! size, salt, nonce base), then the archive (`archive.rs`) encrypted with
//! XChaCha20-Poly1305 in chunks, STREAM-style: chunk `i` uses the nonce
//! `base(19) ‖ i (u32 BE) ‖ last (1 byte)` and the associated data
//! `header ‖ last`, so the header is authenticated, chunks can't be
//! reordered or dropped, and cutting the file at a chunk boundary fails
//! (the new last chunk wasn't sealed as last). The key is derived from
//! the passphrase with Argon2id and lives in memory only.

pub mod archive;
#[cfg(test)]
mod tests;

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::rand_core::RngCore;
use chacha20poly1305::aead::{Aead, KeyInit, OsRng, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};

pub const MAGIC: &[u8; 8] = b"NSVAULT\0";
pub const VERSION: u16 = 1;
pub const EXTENSION: &str = "notesec-vault";
const SALT_LEN: usize = 16;
const NONCE_BASE_LEN: usize = 19;
const TAG_LEN: usize = 16;
pub const HEADER_LEN: usize = 8 + 2 + 4 * 4 + SALT_LEN + NONCE_BASE_LEN;
/// Plaintext per chunk.
pub const CHUNK: u32 = 1 << 20;
/// The biggest graph a vault holds (it is built and checked in memory).
pub const MAX_PLAINTEXT: usize = 2 << 30;
/// The shortest passphrase accepted.
pub const MIN_PASSPHRASE: usize = 12;
const WRONG: &str = "Wrong passphrase, or the vault file was changed or damaged";

/// Argon2id costs: memory in KiB, passes, lanes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Kdf {
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
}

/// What new vaults use: 64 MiB, 3 passes, 1 lane.
pub const DEFAULT_KDF: Kdf = Kdf {
    m_kib: 64 * 1024,
    t: 3,
    p: 1,
};

/// Cheap costs for tests only (the format is the same).
#[cfg(test)]
pub const TEST_KDF: Kdf = Kdf {
    m_kib: 64,
    t: 1,
    p: 1,
};

impl Kdf {
    /// Costs a vault may ask for. A file claiming more is refused before
    /// any work (a denial-of-service guard): at most 1 GiB, 10 passes, 4
    /// lanes.
    pub fn check(self) -> Result<(), String> {
        let ok = (8..=1 << 20).contains(&self.m_kib)
            && (1..=10).contains(&self.t)
            && (1..=4).contains(&self.p)
            && self.m_kib >= 8 * self.p;
        if ok {
            Ok(())
        } else {
            Err(format!(
                "The vault asks for unreasonable key-derivation settings \
                 ({} KiB, {} passes, {} lanes), so it isn't opened",
                self.m_kib, self.t, self.p
            ))
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Header {
    kdf: Kdf,
    chunk: u32,
    salt: [u8; SALT_LEN],
    nonce: [u8; NONCE_BASE_LEN],
}

impl Header {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_be_bytes());
        for n in [self.kdf.m_kib, self.kdf.t, self.kdf.p, self.chunk] {
            out.extend_from_slice(&n.to_be_bytes());
        }
        out.extend_from_slice(&self.salt);
        out.extend_from_slice(&self.nonce);
        out
    }

    fn decode(bytes: &[u8]) -> Result<Header, String> {
        if bytes.len() != HEADER_LEN || &bytes[..8] != MAGIC {
            return Err("Not a NoteSec vault file".into());
        }
        let version = u16::from_be_bytes([bytes[8], bytes[9]]);
        if version != VERSION {
            return Err(format!(
                "This vault is format version {version}; this NoteSec reads version {VERSION}"
            ));
        }
        let n = |i: usize| u32::from_be_bytes(bytes[10 + 4 * i..14 + 4 * i].try_into().unwrap());
        let kdf = Kdf {
            m_kib: n(0),
            t: n(1),
            p: n(2),
        };
        kdf.check()?;
        let chunk = n(3);
        if !(4096..=16 << 20).contains(&chunk) {
            return Err(format!("The vault's chunk size ({chunk}) is unreasonable"));
        }
        let mut salt = [0; SALT_LEN];
        salt.copy_from_slice(&bytes[26..26 + SALT_LEN]);
        let mut nonce = [0; NONCE_BASE_LEN];
        nonce.copy_from_slice(&bytes[26 + SALT_LEN..]);
        Ok(Header {
            kdf,
            chunk,
            salt,
            nonce,
        })
    }
}

/// Overwrite a secret before letting it go (best effort: no unsafe, so
/// copies the allocator or the OS made earlier can't be reached).
pub fn wipe(buf: &mut [u8]) {
    buf.fill(0);
    std::hint::black_box(&buf);
}

/// Wipe a `String`'s bytes and empty it.
pub fn wipe_string(s: &mut String) {
    let mut bytes = std::mem::take(s).into_bytes();
    wipe(&mut bytes);
}

fn cipher(passphrase: &str, header: &Header) -> Result<XChaCha20Poly1305, String> {
    let params = Params::new(header.kdf.m_kib, header.kdf.t, header.kdf.p, Some(32))
        .map_err(|err| format!("Key derivation settings: {err}"))?;
    let mut key = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase.as_bytes(), &header.salt, &mut key)
        .map_err(|err| format!("Key derivation failed: {err}"))?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&key));
    wipe(&mut key);
    Ok(cipher)
}

fn nonce(base: &[u8; NONCE_BASE_LEN], index: u32, last: bool) -> XNonce {
    let mut n = [0u8; 24];
    n[..NONCE_BASE_LEN].copy_from_slice(base);
    n[NONCE_BASE_LEN..23].copy_from_slice(&index.to_be_bytes());
    n[23] = last as u8;
    *XNonce::from_slice(&n)
}

/// Encrypt `plain` into `out` (a fresh salt and nonce from the OS).
pub fn seal(plain: &[u8], passphrase: &str, kdf: Kdf, out: &mut impl Write) -> Result<(), String> {
    kdf.check()?;
    if plain.len() > MAX_PLAINTEXT {
        return Err("The graph is too big for a vault (2 GiB at most)".into());
    }
    let mut header = Header {
        kdf,
        chunk: CHUNK,
        salt: [0; SALT_LEN],
        nonce: [0; NONCE_BASE_LEN],
    };
    OsRng.fill_bytes(&mut header.salt);
    OsRng.fill_bytes(&mut header.nonce);
    let cipher = cipher(passphrase, &header)?;
    let head = header.encode();
    let io = |err: io::Error| format!("Could not write the vault: {err}");
    out.write_all(&head).map_err(io)?;
    let chunks: Vec<&[u8]> = if plain.is_empty() {
        vec![&[]]
    } else {
        plain.chunks(CHUNK as usize).collect()
    };
    let count = chunks.len();
    for (i, chunk) in chunks.into_iter().enumerate() {
        let last = i + 1 == count;
        let mut aad = head.clone();
        aad.push(last as u8);
        let index = u32::try_from(i).map_err(|_| "Too many chunks".to_string())?;
        let sealed = cipher
            .encrypt(
                &nonce(&header.nonce, index, last),
                Payload {
                    msg: chunk,
                    aad: &aad,
                },
            )
            .map_err(|_| "Encryption failed".to_string())?;
        out.write_all(&sealed).map_err(io)?;
    }
    Ok(())
}

/// Read as much as fits in `buf` (less only at the end of the input).
fn read_full(input: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match input.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(n)
}

/// Decrypt and authenticate a whole vault. Any change to the header or
/// the chunks, a missing or extra chunk, or a wrong passphrase fails.
pub fn open(input: impl Read, passphrase: &str) -> Result<Vec<u8>, String> {
    let mut input = BufReader::new(input);
    let io = |err: io::Error| format!("Could not read the vault: {err}");
    let mut head = vec![0u8; HEADER_LEN];
    if read_full(&mut input, &mut head).map_err(io)? != HEADER_LEN {
        return Err("Not a NoteSec vault file".into());
    }
    let header = Header::decode(&head)?;
    let cipher = cipher(passphrase, &header)?;
    let mut plain = Vec::new();
    let mut buf = vec![0u8; header.chunk as usize + TAG_LEN];
    for index in 0u32.. {
        let n = read_full(&mut input, &mut buf).map_err(io)?;
        let last = input.fill_buf().map_err(io)?.is_empty();
        if n < TAG_LEN || (!last && n != buf.len()) {
            return Err(WRONG.into());
        }
        let mut aad = head.clone();
        aad.push(last as u8);
        let chunk = cipher
            .decrypt(
                &nonce(&header.nonce, index, last),
                Payload {
                    msg: &buf[..n],
                    aad: &aad,
                },
            )
            .map_err(|_| WRONG.to_string())?;
        if plain.len() + chunk.len() > MAX_PLAINTEXT {
            return Err("The vault is too big".into());
        }
        plain.extend_from_slice(&chunk);
        if last {
            return Ok(plain);
        }
    }
    Err(WRONG.into())
}

/// Write the graph at `root` as a vault at `dest` (via a temp file next
/// to it, renamed into place). Returns how many files went in.
pub fn export(root: &Path, dest: &Path, passphrase: &str, kdf: Kdf) -> Result<usize, String> {
    let entries =
        archive::collect(root).map_err(|err| format!("Could not read the graph: {err}"))?;
    let mut packed = archive::pack(&entries)?;
    let name = dest.file_name().and_then(|n| n.to_str()).unwrap_or("vault");
    let tmp = dest.with_file_name(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|err| format!("Could not write {}: {err}", tmp.display()))?;
        let mut out = io::BufWriter::new(&mut file);
        seal(&packed, passphrase, kdf, &mut out)?;
        out.flush()
            .map_err(|err| format!("Could not write the vault: {err}"))?;
        drop(out);
        file.sync_all()
            .map_err(|err| format!("Could not write the vault: {err}"))?;
        fs::rename(&tmp, dest).map_err(|err| format!("Could not save {}: {err}", dest.display()))
    })();
    wipe(&mut packed);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map(|()| entries.len())
}

/// Decrypt the vault `file` into the empty folder `dest`. Everything is
/// decrypted and checked before anything is written; files go into a
/// hidden folder in `dest` first and are moved out of it at the end.
pub fn import(file: &Path, dest: &Path, passphrase: &str) -> Result<usize, String> {
    let mut items =
        fs::read_dir(dest).map_err(|err| format!("Can't use {}: {err}", dest.display()))?;
    if items.next().is_some() {
        return Err(format!(
            "{} isn't empty: pick a new, empty folder (a vault is never merged into notes)",
            dest.display()
        ));
    }
    let input =
        fs::File::open(file).map_err(|err| format!("Could not open {}: {err}", file.display()))?;
    let mut plain = open(input, passphrase)?;
    let entries = archive::unpack(&plain);
    wipe(&mut plain);
    let entries = entries?;
    let staging = dest.join(format!(".notesec-import-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        fs::create_dir(&staging)?;
        archive::write_all(&staging, &entries)?;
        for item in fs::read_dir(&staging)? {
            let item = item?;
            fs::rename(item.path(), dest.join(item.file_name()))?;
        }
        fs::remove_dir(&staging)
    })();
    if let Err(err) = result {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!("Could not write the notes: {err}"));
    }
    Ok(entries.len())
}
