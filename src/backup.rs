//! Password-encrypted backups (scrypt + AES-256-GCM).
//!
//! The envelope is byte-compatible with Relayline's account backups, so a backup made by
//! either tool restores in the other.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

pub const MIN_PASSWORD: usize = 12;
const MAX_PASSWORD: usize = 256;
pub const MAX_BACKUP: usize = 1_500_000;
const FORMAT: &str = "relayline-account-backup";
const HEADER: &[u8] = b"relayline-account-backup/v1";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub label: String,
    pub token: String,
    pub parked: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    format: String,
    version: u32,
    salt: String,
    nonce: String,
    tag: String,
    data: String,
}

pub fn check_password(password: &str) -> Result<()> {
    let length = password.chars().count();
    if length < MIN_PASSWORD || password.len() > MAX_PASSWORD {
        bail!("Backup passwords must be {MIN_PASSWORD}-{MAX_PASSWORD} characters.");
    }
    Ok(())
}

fn key_for(password: &str, salt: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
    let params = scrypt::Params::new(16, 8, 1, 32).map_err(|error| anyhow!("{error}"))?;
    let mut key = Zeroizing::new([0u8; 32]);
    scrypt::scrypt(password.as_bytes(), salt, &params, key.as_mut())
        .map_err(|error| anyhow!("{error}"))?;
    Ok(key)
}

pub fn encrypt(entries: &[Entry], password: &str) -> Result<String> {
    check_password(password)?;
    let mut plain = serde_json::to_vec(entries)?;
    if plain.len() as f64 > (MAX_BACKUP - 512) as f64 * 0.75 {
        plain.zeroize();
        bail!("This account pool is too large for a single backup.");
    }
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let key = key_for(password, &salt)?;
    let cipher = Aes256Gcm::new_from_slice(key.as_ref()).expect("32-byte key");
    let sealed = cipher.encrypt(
        Nonce::from_slice(&nonce),
        Payload {
            msg: &plain,
            aad: HEADER,
        },
    );
    plain.zeroize();
    let mut sealed = sealed.map_err(|_| anyhow!("Encryption failed."))?;
    let tag = sealed.split_off(sealed.len() - 16);
    Ok(serde_json::to_string(&Envelope {
        format: FORMAT.into(),
        version: 1,
        salt: B64.encode(salt),
        nonce: B64.encode(nonce),
        tag: B64.encode(tag),
        data: B64.encode(sealed),
    })?)
}

pub fn decrypt(text: &str, password: &str) -> Result<Vec<Entry>> {
    let not_backup =
        || anyhow!("This is not a claude-oauth or Relayline encrypted account backup.");
    if text.len() > MAX_BACKUP {
        return Err(not_backup());
    }
    let envelope: Envelope = serde_json::from_str(text.trim()).map_err(|_| not_backup())?;
    let decode = |value: &str, length: Option<usize>| {
        let bytes = B64.decode(value).map_err(|_| not_backup())?;
        if length.is_some_and(|length| bytes.len() != length) {
            return Err(not_backup());
        }
        Ok(bytes)
    };
    if envelope.format != FORMAT || envelope.version != 1 {
        return Err(not_backup());
    }
    let salt = decode(&envelope.salt, Some(16))?;
    let nonce = decode(&envelope.nonce, Some(12))?;
    let mut sealed = decode(&envelope.data, None)?;
    sealed.extend(decode(&envelope.tag, Some(16))?);
    let key = key_for(password, &salt)?;
    let cipher = Aes256Gcm::new_from_slice(key.as_ref()).expect("32-byte key");
    let unlock_failed = || {
        anyhow!("Could not unlock this backup. Check the password and that the file is complete.")
    };
    let plain = Zeroizing::new(
        cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &sealed,
                    aad: HEADER,
                },
            )
            .map_err(|_| unlock_failed())?,
    );
    let entries: Vec<Entry> = serde_json::from_slice(&plain).map_err(|_| unlock_failed())?;
    if entries.iter().any(|entry| {
        !crate::store::valid_token(&entry.token) || crate::store::check_name(&entry.label).is_err()
    }) {
        return Err(unlock_failed());
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWORD: &str = "correct horse battery";

    fn entries() -> Vec<Entry> {
        vec![
            Entry {
                label: "Work".into(),
                token: "sk-ant-oat01-aaaa".into(),
                parked: false,
            },
            Entry {
                label: "Side".into(),
                token: "sk-ant-oat01-bbbb".into(),
                parked: true,
            },
        ]
    }

    #[test]
    fn round_trips_and_rejects_a_wrong_password() {
        let text = encrypt(&entries(), PASSWORD).unwrap();
        assert!(!text.contains("sk-ant"));
        assert_eq!(decrypt(&text, PASSWORD).unwrap(), entries());
        assert!(
            decrypt(&text, "wrong password!!")
                .unwrap_err()
                .to_string()
                .contains("Could not unlock")
        );
    }

    #[test]
    fn rejects_tampering_and_short_passwords() {
        let text = encrypt(&entries(), PASSWORD).unwrap();
        let mut envelope: serde_json::Value = serde_json::from_str(&text).unwrap();
        envelope["format"] = "something-else".into();
        assert!(decrypt(&envelope.to_string(), PASSWORD).is_err());
        assert!(decrypt("not json", PASSWORD).is_err());
        assert!(encrypt(&entries(), "short").is_err());
    }

    /// Produced by Relayline 1.3.0's `encryptAccounts` with the password below.
    #[test]
    fn restores_a_relayline_backup() {
        let fixture = include_str!("../tests/fixtures/relayline-backup.json");
        let entries = decrypt(fixture, "relayline fixture password").unwrap();
        assert_eq!(
            entries,
            vec![
                Entry {
                    label: "Fixture work".into(),
                    token: "sk-ant-oat01-fixture-work".into(),
                    parked: false
                },
                Entry {
                    label: "Fixture paused".into(),
                    token: "sk-ant-oat01-fixture-paused".into(),
                    parked: true
                },
            ]
        );
    }
}
