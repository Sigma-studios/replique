//! Encryption at rest, random secrets and hashes.
//!
//! The Hetzner token, the Discord webhook and the passwords of sub-accounts
//! waiting to be paired are sealed with a key kept in `<state_dir>/master.key`,
//! so a copy of the database alone gives nothing away.

use std::fmt;
use std::path::Path;

use anyhow::{Context, anyhow};
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand::seq::SliceRandom;
use rand::{Rng, RngCore};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

const NONCE_LEN: usize = 12;

pub struct SecretBox(ChaCha20Poly1305);

impl SecretBox {
    pub fn load_or_create(path: &Path) -> anyhow::Result<SecretBox> {
        if !path.exists() {
            let mut key = [0u8; 32];
            rand::rng().fill_bytes(&mut key);
            write_private(path, hex::encode(key).as_bytes())?;
        }
        let raw = Zeroizing::new(
            std::fs::read_to_string(path)
                .with_context(|| format!("reading master key {}", path.display()))?,
        );
        let key = Zeroizing::new(hex::decode(raw.trim()).context("master key is not hex")?);
        if key.len() != 32 {
            return Err(anyhow!("master key must be 32 bytes"));
        }
        Ok(SecretBox(ChaCha20Poly1305::new(Key::from_slice(&key))))
    }

    pub fn seal(&self, plaintext: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::rng().fill_bytes(&mut nonce);
        let mut out = nonce.to_vec();
        out.extend(
            self.0
                .encrypt(Nonce::from_slice(&nonce), plaintext)
                .expect("encryption cannot fail"),
        );
        out
    }

    pub fn open(&self, sealed: &[u8]) -> anyhow::Result<Zeroizing<Vec<u8>>> {
        if sealed.len() < NONCE_LEN {
            return Err(anyhow!("sealed value too short"));
        }
        let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
        self.0
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map(Zeroizing::new)
            .map_err(|_| anyhow!("cannot decrypt a stored secret (was master.key replaced?)"))
    }

    pub fn open_string(&self, sealed: &[u8]) -> anyhow::Result<Secret> {
        let bytes = self.open(sealed)?;
        Ok(Secret::new(String::from_utf8(bytes.to_vec())?))
    }
}

/// Writes a file only its owner can read, creating its folder.
pub fn write_private(path: &Path, content: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    file.write_all(content)?;
    Ok(())
}

/// A string that must never reach a log: its `Debug` and `Display` print a
/// placeholder, and its memory is wiped when dropped. Passwords typed to unlock
/// a repository, sub-account passwords and tokens travel in it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Zeroizing<String>);

impl Secret {
    pub fn new(value: String) -> Secret {
        Secret(Zeroizing::new(value))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(••••)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("••••")
    }
}

impl<'de> serde::Deserialize<'de> for Secret {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d).map(Secret::new)
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// `n` random bytes in unpadded base64url.
pub fn random_b64(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A device report token: `rpl_` and 32 random bytes (see `replique_proto`).
pub fn new_report_token() -> Secret {
    Secret::new(format!(
        "{}{}",
        replique_proto::TOKEN_PREFIX,
        random_b64(replique_proto::TOKEN_RANDOM_BYTES)
    ))
}

/// A pairing code, shown as `XXXX-XXXX`.
pub fn new_pairing_code() -> String {
    let mut rng = rand::rng();
    let alphabet = replique_proto::PAIRING_CODE_ALPHABET;
    let raw: String = (0..replique_proto::PAIRING_CODE_LEN)
        .map(|_| alphabet[rng.random_range(0..alphabet.len())] as char)
        .collect();
    format!("{}-{}", &raw[..4], &raw[4..])
}

/// A sub-account password nobody will type. Hetzner's policy wants 12 to 128
/// characters with an upper and a lower case letter, a digit and one of its
/// special characters; the specials picked here need no quoting in a shell.
pub fn new_box_password() -> Secret {
    const LOWER: &[u8] = b"abcdefghijkmnopqrstuvwxyz";
    const UPPER: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ";
    const DIGIT: &[u8] = b"23456789";
    const SPECIAL: &[u8] = b"-_.@+";
    let mut rng = rand::rng();
    let all: Vec<u8> = [LOWER, UPPER, DIGIT, SPECIAL].concat();
    let pick = |rng: &mut rand::rngs::ThreadRng, set: &[u8]| set[rng.random_range(0..set.len())];
    // One of each class guarantees the policy; the first character is a letter
    // so no tool mistakes the password for an option.
    let mut tail = vec![
        pick(&mut rng, UPPER),
        pick(&mut rng, DIGIT),
        pick(&mut rng, SPECIAL),
    ];
    tail.extend((0..25).map(|_| pick(&mut rng, &all)));
    tail.shuffle(&mut rng);
    let mut chars = vec![pick(&mut rng, LOWER)];
    chars.extend(tail);
    Secret::new(String::from_utf8(chars).expect("ASCII"))
}

/// Constant-time comparison for tokens of equal length.
pub fn eq_ct(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_well_formed() {
        let t = new_report_token();
        assert!(replique_proto::is_well_formed_token(t.expose()));
        let code = new_pairing_code();
        assert_eq!(code.len(), 9);
        assert_eq!(replique_proto::normalize_pairing_code(&code).len(), 8);
    }

    #[test]
    fn box_passwords_follow_hetzner_policy() {
        for _ in 0..200 {
            let p = new_box_password();
            let p = p.expose();
            assert!(p.len() >= 12);
            assert!(p.chars().any(|c| c.is_ascii_lowercase()));
            assert!(p.chars().any(|c| c.is_ascii_uppercase()));
            assert!(p.chars().any(|c| c.is_ascii_digit()));
            assert!(p.chars().any(|c| "-_.@+".contains(c)));
        }
    }

    #[test]
    fn secrets_do_not_print() {
        let s = Secret::new("hunter2-very-secret".into());
        assert!(!format!("{s:?} {s}").contains("hunter2"));
    }

    #[test]
    fn seal_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let b = SecretBox::load_or_create(&dir.path().join("master.key")).unwrap();
        let sealed = b.seal(b"token");
        assert_eq!(&**b.open(&sealed).unwrap(), b"token");
        let again = SecretBox::load_or_create(&dir.path().join("master.key")).unwrap();
        assert_eq!(&**again.open(&sealed).unwrap(), b"token");
    }
}
