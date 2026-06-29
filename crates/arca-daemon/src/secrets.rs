//! Secrets store. Decrypts `/etc/arca/secrets.age` using `age` x25519 identities
//! from `/etc/arca/secrets.key`. Content is TOML: one `key = "value"` per line.
//! Decrypted bytes are zeroized on drop (via `secrecy::SecretString`).

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use age::IdentityFileEntry;
use anyhow::{Context, Result, anyhow, bail};
use secrecy::{ExposeSecret, SecretString};

#[derive(Default)]
pub struct Secrets {
    map: HashMap<String, SecretString>,
}

impl Secrets {
    /// Load and decrypt. Missing file → empty store (logged). Malformed file → error
    /// (no silent fallback, per the design spec).
    pub fn load(age_path: Option<&Path>, key_path: Option<&Path>) -> Result<Self> {
        let (Some(age_path), Some(key_path)) = (age_path, key_path) else {
            tracing::info!("no secrets file configured; running without secrets");
            return Ok(Self::default());
        };
        if !age_path.exists() {
            tracing::warn!(path = %age_path.display(), "secrets file missing; running without secrets");
            return Ok(Self::default());
        }
        if !key_path.exists() {
            return Err(anyhow!(
                "secrets file present at {} but key file missing at {}",
                age_path.display(),
                key_path.display()
            ));
        }

        // The age identity must be 0400-ish: refuse a group/other-accessible key
        // (e.g. left 0644 after a botched install) rather than silently accepting
        // it and defeating the at-rest protection (the design spec: key mode 0400).
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(key_path)
                .with_context(|| format!("stat key {}", key_path.display()))?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                bail!(
                    "secrets.key {} is group/other-accessible (mode {:o}); chmod 0400",
                    key_path.display(),
                    mode & 0o7777
                );
            }
        }

        let key_text = std::fs::read_to_string(key_path)
            .with_context(|| format!("read key {}", key_path.display()))?;
        let id_file = age::IdentityFile::from_buffer(std::io::BufReader::new(key_text.as_bytes()))
            .map_err(|e| anyhow!("parse age identities: {e}"))?;
        let identities: Vec<age::x25519::Identity> = id_file
            .into_identities()
            .into_iter()
            .map(|entry| match entry {
                IdentityFileEntry::Native(id) => Ok(id),
                #[allow(unreachable_patterns)]
                _ => Err(anyhow!(
                    "unsupported identity type (only native x25519 is supported)"
                )),
            })
            .collect::<Result<_>>()?;
        if identities.is_empty() {
            bail!("no x25519 identities in {}", key_path.display());
        }

        let blob =
            std::fs::read(age_path).with_context(|| format!("read {}", age_path.display()))?;
        let decryptor =
            age::Decryptor::new(blob.as_slice()).map_err(|e| anyhow!("age decryptor: {e}"))?;
        let recipients_decryptor = match decryptor {
            age::Decryptor::Recipients(d) => d,
            age::Decryptor::Passphrase(_) => {
                bail!("passphrase-encrypted secrets are not supported")
            }
        };
        let id_refs = identities.iter().map(|i| i as &dyn age::Identity);
        let mut reader = recipients_decryptor
            .decrypt(id_refs)
            .map_err(|e| anyhow!("age decrypt: {e}"))?;
        let mut plaintext = Vec::new();
        reader
            .read_to_end(&mut plaintext)
            .context("read decrypted secrets")?;

        let text = String::from_utf8(plaintext).map_err(|_| anyhow!("secrets not UTF-8"))?;
        let parsed: toml::Table = toml::from_str(&text).context("parse secrets toml")?;
        let mut map = HashMap::new();
        for (k, v) in parsed {
            match v {
                toml::Value::String(s) => {
                    map.insert(k, SecretString::new(s));
                }
                other => {
                    return Err(anyhow!(
                        "secret {k} must be a string, got {}",
                        other.type_str()
                    ));
                }
            }
        }
        tracing::info!(count = map.len(), "secrets loaded");
        Ok(Self { map })
    }

    pub fn get(&self, key: &str) -> Option<&SecretString> {
        self.map.get(key)
    }

    pub fn require(&self, key: &str) -> Result<&str> {
        self.map
            .get(key)
            .map(|s| s.expose_secret().as_str())
            .ok_or_else(|| anyhow!("missing secret: {key}"))
    }

    /// Like [`require`](Self::require) but returns an owned, drop-zeroized
    /// [`SecretString`] for a provider to hold for its lifetime — instead of a
    /// bare `String` that lingers unzeroized in process memory (and in any core
    /// dump). The provider `expose_secret()`s it only at the point it builds a
    /// request header/body.
    pub fn require_owned(&self, key: &str) -> Result<SecretString> {
        self.require(key).map(|s| SecretString::new(s.to_string()))
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Test helper: build a Secrets store without touching age.
    pub fn for_test(pairs: &[(&str, &str)]) -> Self {
        Self {
            map: pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), SecretString::new((*v).to_string())))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn missing_files_returns_empty() {
        let s = Secrets::load(None, None).unwrap();
        assert!(s.is_empty());
    }

    #[test]
    fn roundtrip_via_age() {
        let dir = tempdir().unwrap();
        let key_path = dir.path().join("secrets.key");
        let age_path = dir.path().join("secrets.age");

        let identity = age::x25519::Identity::generate();
        let pubkey = identity.to_public();
        std::fs::write(&key_path, identity.to_string().expose_secret()).unwrap();
        // load() refuses a group/other-accessible key, so mirror the real 0400.
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o400)).unwrap();
        }

        let plaintext = b"plaid_navy_fed_access_token = \"abc-123\"\nmercury_main = \"merc-xyz\"\n";
        let recipients: Vec<Box<dyn age::Recipient + Send>> = vec![Box::new(pubkey)];
        let encryptor = age::Encryptor::with_recipients(recipients).unwrap();
        let mut ciphertext = Vec::new();
        let mut w = encryptor.wrap_output(&mut ciphertext).unwrap();
        w.write_all(plaintext).unwrap();
        w.finish().unwrap();
        std::fs::write(&age_path, ciphertext).unwrap();

        let s = Secrets::load(Some(&age_path), Some(&key_path)).unwrap();
        assert_eq!(s.require("plaid_navy_fed_access_token").unwrap(), "abc-123");
        assert_eq!(s.require("mercury_main").unwrap(), "merc-xyz");
        assert!(s.get("missing").is_none());
    }
}
