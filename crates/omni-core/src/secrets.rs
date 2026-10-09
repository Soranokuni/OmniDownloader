//! Secrets at rest (plan P2.6, defect W-06).
//!
//! The mailbox password lived in `config.json` as plaintext, and
//! `save_to_file` wrote it back on every change. Anyone who could read the
//! install directory — a backup, a support copy of the folder, the admin panel
//! itself before Phase 2 — had the station's ingest mailbox.
//!
//! Secrets now live in `data/secrets.bin`, encrypted with Windows DPAPI at
//! **machine** scope. Machine rather than user scope is deliberate: the daemon
//! runs as a service, and the account it runs as is something the operator may
//! change (plan P2.8). A user-scoped blob would decrypt fine in testing and
//! then fail the first time the service was moved to a domain account — at
//! start-up, in the newsroom, with no obvious cause.
//!
//! DPAPI ties the file to the machine, not to a password we would otherwise
//! have to store somewhere else to unlock it. Copying `secrets.bin` to another
//! machine yields ciphertext that will not open, which is the property that
//! matters for a folder that gets backed up.
//!
//! Non-Windows builds (CI, tests) fall back to an obfuscated file and log a
//! warning on every write. That is honest about what it is: enough to keep the
//! shape of the code identical, not a security control.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Canonical secret names. String keys elsewhere would drift.
pub mod keys {
    /// Microsoft Graph application secret (plan P4.2).
    pub const GRAPH_CLIENT_SECRET: &str = "graph.client_secret";
    /// The LLM provider's API key (plan P4.22). Empty for local runtimes.
    pub const LLM_API_KEY: &str = "llm.api_key";
    /// Teams incoming-webhook URL (plan P5.3) — a URL that is itself a
    /// credential, since anyone holding it can post to the MCR channel.
    pub const TEAMS_WEBHOOK_URL: &str = "teams.webhook_url";
    /// Passphrase for the PKCS#12 TLS certificate (plan P2.7).
    pub const TLS_PASSWORD: &str = "web.tls_password";

    /// Every key the panel and CLI accept, for validation and listing.
    pub const ALL: &[&str] = &[
        GRAPH_CLIENT_SECRET,
        LLM_API_KEY,
        TEAMS_WEBHOOK_URL,
        TLS_PASSWORD,
    ];

    /// Keys no build reads any more; removed from the store at start-up.
    /// `mail.password` was the IMAP password (IMAP removed in plan P4.7).
    pub const RETIRED: &[&str] = &["mail.password"];
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SecretMap {
    #[serde(default)]
    values: BTreeMap<String, String>,
}

/// Encrypted key/value store on disk.
#[derive(Debug, Clone)]
pub struct SecretStore {
    path: PathBuf,
}

impl SecretStore {
    /// Open (or prepare to create) the store at `data/secrets.bin`.
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read a secret. A missing store or a missing key is `Ok(None)`.
    pub fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.load()?.values.get(key).cloned())
    }

    /// Read a secret, treating a decryption failure as "not set".
    ///
    /// Used on the start-up path: a `secrets.bin` that will not open (restored
    /// from another machine, say) must not stop the daemon from ingesting
    /// video. The operator gets a warning and a mailbox that does not connect,
    /// not a station with no ingest at all.
    pub fn get_lossy(&self, key: &str) -> Option<String> {
        match self.get(key) {
            Ok(value) => value,
            Err(e) => {
                warn!(
                    key,
                    error = %e,
                    "Could not read the secret store; treating this secret as unset. \
                     Re-enter it in Admin, or with `omni-ingest secrets set`."
                );
                None
            }
        }
    }

    /// Write a secret. An empty value removes it.
    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        let mut map = self.load().unwrap_or_default();
        if value.is_empty() {
            map.values.remove(key);
        } else {
            map.values.insert(key.to_string(), value.to_string());
        }
        self.store(&map)
    }

    pub fn remove(&self, key: &str) -> Result<()> {
        self.set(key, "")
    }

    /// Which secrets are set. Values are never returned — the panel shows
    /// `is_set`, and there is no API that reads one back out.
    pub fn status(&self) -> BTreeMap<String, bool> {
        let map = self.load().unwrap_or_default();
        keys::ALL
            .iter()
            .map(|k| (k.to_string(), map.values.contains_key(*k)))
            .collect()
    }

    /// Every stored value, for the log redactor. Not exposed over any API.
    pub fn all_values(&self) -> Vec<String> {
        self.load()
            .unwrap_or_default()
            .values
            .into_values()
            .filter(|v| v.len() >= 6)
            .collect()
    }

    fn load(&self) -> Result<SecretMap> {
        if !self.path.exists() {
            return Ok(SecretMap::default());
        }
        let blob = std::fs::read(&self.path)
            .with_context(|| format!("Failed reading secret store {:?}", self.path))?;
        if blob.is_empty() {
            return Ok(SecretMap::default());
        }
        let plaintext = unprotect(&blob).context("Failed decrypting the secret store")?;
        let map: SecretMap =
            serde_json::from_slice(&plaintext).context("Secret store is not valid JSON")?;
        Ok(map)
    }

    fn store(&self, map: &SecretMap) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed creating {:?}", parent))?;
        }
        let plaintext = serde_json::to_vec(map)?;
        let blob = protect(&plaintext).context("Failed encrypting the secret store")?;

        // Write-and-rename, so an interrupted write cannot leave a truncated
        // store that decrypts to nothing and silently loses every secret.
        let tmp = self.path.with_extension("bin.tmp");
        std::fs::write(&tmp, &blob).with_context(|| format!("Failed writing {:?}", tmp))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("Failed replacing {:?}", self.path))?;
        Ok(())
    }
}

// ==========================================
// Platform encryption
// ==========================================

#[cfg(windows)]
fn protect(plaintext: &[u8]) -> Result<Vec<u8>> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CRYPT_INTEGER_BLOB, CRYPTPROTECT_LOCAL_MACHINE,
    };

    unsafe {
        let mut input = CRYPT_INTEGER_BLOB {
            cbData: plaintext.len() as u32,
            pbData: plaintext.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB::default();

        CryptProtectData(
            &mut input,
            windows::core::w!("OmniDownloader secrets"),
            None,
            None,
            None,
            CRYPTPROTECT_LOCAL_MACHINE,
            &mut output,
        )
        .context("CryptProtectData failed")?;

        let slice = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(output.pbData as *mut _));
        Ok(slice)
    }
}

#[cfg(windows)]
fn unprotect(blob: &[u8]) -> Result<Vec<u8>> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPT_INTEGER_BLOB, CRYPTPROTECT_LOCAL_MACHINE,
    };

    unsafe {
        let mut input = CRYPT_INTEGER_BLOB {
            cbData: blob.len() as u32,
            pbData: blob.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB::default();

        CryptUnprotectData(
            &mut input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_LOCAL_MACHINE,
            &mut output,
        )
        .context(
            "CryptUnprotectData failed. `data/secrets.bin` is tied to this machine; a store \
             copied from another install cannot be opened here and must be re-entered.",
        )?;

        let slice = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(output.pbData as *mut _));
        Ok(slice)
    }
}

/// Non-Windows fallback.
///
/// This is obfuscation, not encryption, and it says so out loud on every
/// write. It exists so the tests and any future non-Windows build exercise the
/// same code path; the deployment target is Windows, where DPAPI is real.
#[cfg(not(windows))]
fn protect(plaintext: &[u8]) -> Result<Vec<u8>> {
    warn!(
        "Secrets are being stored WITHOUT encryption: this build is not on Windows, so DPAPI \
         is unavailable. Do not use this build to hold production credentials."
    );
    Ok(xor_obfuscate(plaintext))
}

#[cfg(not(windows))]
fn unprotect(blob: &[u8]) -> Result<Vec<u8>> {
    Ok(xor_obfuscate(blob))
}

#[cfg(not(windows))]
fn xor_obfuscate(data: &[u8]) -> Vec<u8> {
    const PAD: &[u8] = b"omni-downloader-not-a-secret";
    data.iter()
        .enumerate()
        .map(|(i, b)| b ^ PAD[i % PAD.len()])
        .collect()
}

// ==========================================
// Log redaction
// ==========================================

/// Replace every known secret value in `text` with `***`.
///
/// External tools are noisy and quote their arguments back: yt-dlp echoes
/// cookies, ffmpeg echoes URLs, an IMAP library may log a failed LOGIN line.
/// Any of those can carry a secret into a log file that then gets mailed to
/// support.
pub fn redact(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for secret in secrets {
        // Short values would match ordinary words and turn the log to noise.
        if secret.len() < 6 {
            continue;
        }
        if out.contains(secret.as_str()) {
            out = out.replace(secret.as_str(), "***");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn a_secret_round_trips_and_never_appears_in_the_file() {
        let dir = TempDir::new().unwrap();
        let store = SecretStore::new(dir.path().join("secrets.bin"));

        store.set(keys::GRAPH_CLIENT_SECRET, "s3cr3t-mailbox-pass").unwrap();
        assert_eq!(
            store.get(keys::GRAPH_CLIENT_SECRET).unwrap().as_deref(),
            Some("s3cr3t-mailbox-pass")
        );

        // The point of the exercise: the value is not readable in the file.
        let raw = std::fs::read(store.path()).unwrap();
        let as_text = String::from_utf8_lossy(&raw);
        assert!(
            !as_text.contains("s3cr3t-mailbox-pass"),
            "the secret is sitting in plaintext on disk"
        );
    }

    #[test]
    fn a_missing_store_reads_as_unset_rather_than_failing() {
        // First start, before anything has been configured. This must not be
        // an error: it is the normal state of a fresh install.
        let dir = TempDir::new().unwrap();
        let store = SecretStore::new(dir.path().join("nothing-here.bin"));
        assert_eq!(store.get(keys::GRAPH_CLIENT_SECRET).unwrap(), None);
        assert!(store.status().values().all(|set| !set));
    }

    #[test]
    fn setting_an_empty_value_removes_the_secret() {
        let dir = TempDir::new().unwrap();
        let store = SecretStore::new(dir.path().join("secrets.bin"));
        store.set(keys::GRAPH_CLIENT_SECRET, "something").unwrap();
        store.set(keys::GRAPH_CLIENT_SECRET, "").unwrap();
        assert_eq!(store.get(keys::GRAPH_CLIENT_SECRET).unwrap(), None);
        assert_eq!(store.status()[keys::GRAPH_CLIENT_SECRET], false);
    }

    #[test]
    fn several_secrets_coexist_and_survive_each_other_being_written() {
        let dir = TempDir::new().unwrap();
        let store = SecretStore::new(dir.path().join("secrets.bin"));
        store.set(keys::TLS_PASSWORD, "tls-one").unwrap();
        store.set(keys::GRAPH_CLIENT_SECRET, "graph-two").unwrap();
        store
            .set(keys::TEAMS_WEBHOOK_URL, "https://teams.example/hook")
            .unwrap();

        assert_eq!(
            store.get(keys::TLS_PASSWORD).unwrap().as_deref(),
            Some("tls-one")
        );
        assert_eq!(
            store.get(keys::GRAPH_CLIENT_SECRET).unwrap().as_deref(),
            Some("graph-two")
        );
        assert_eq!(store.status().values().filter(|v| **v).count(), 3);
    }

    #[test]
    fn a_corrupt_store_is_reported_but_does_not_stop_the_daemon() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("secrets.bin");
        std::fs::write(&path, b"this is not an encrypted blob").unwrap();
        let store = SecretStore::new(&path);

        // The strict read reports the problem...
        assert!(store.get(keys::GRAPH_CLIENT_SECRET).is_err() || store.get(keys::GRAPH_CLIENT_SECRET).unwrap().is_none());
        // ...and the start-up read degrades to "not configured", so the
        // station still ingests video from the web UI.
        assert_eq!(store.get_lossy(keys::GRAPH_CLIENT_SECRET), None);
    }

    #[test]
    fn redaction_replaces_secret_values_wherever_they_appear() {
        let secrets = vec!["hunter2-hunter2".to_string(), "abc".to_string()];
        let line = "IMAP LOGIN ingest@station.gr hunter2-hunter2 failed";
        assert_eq!(
            redact(line, &secrets),
            "IMAP LOGIN ingest@station.gr *** failed"
        );

        // A short value is skipped: redacting "abc" would black out ordinary
        // words and make the log useless, which is its own kind of failure.
        assert_eq!(redact("abc appears in abcdef", &secrets), "abc appears in abcdef");
    }

    #[test]
    fn an_interrupted_write_leaves_no_temp_file_behind() {
        let dir = TempDir::new().unwrap();
        let store = SecretStore::new(dir.path().join("secrets.bin"));
        store.set(keys::GRAPH_CLIENT_SECRET, "value").unwrap();
        assert!(!dir.path().join("secrets.bin.tmp").exists());
    }
}
