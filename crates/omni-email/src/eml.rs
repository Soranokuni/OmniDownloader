//! Saved `.eml` files as a mail source (plan P7.9).
//!
//! `omni-ingest mail-ingest --eml …` feeds files through the watcher's own
//! path (parse, queue, save attachments, record the mail with its text), so
//! a mail can be replayed into the queue without the mailbox: to test the
//! parser and the panels end to end, or to queue a mail that reached MCR
//! some other way. Nothing here touches a mailbox.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use mailparse::{parse_mail, ParsedMail};

use crate::mail::InboundMail;
use crate::source::{MailGone, MailHeader, MailHealth, MailOutcome, MailSource};

/// Raw messages by id; the id is whatever the caller names a file by.
pub struct EmlSource {
    raw: HashMap<String, Vec<u8>>,
}

impl EmlSource {
    pub fn new(files: impl IntoIterator<Item = (String, Vec<u8>)>) -> Self {
        Self { raw: files.into_iter().collect() }
    }

    /// Read `paths`; each file's id is its path as given.
    pub fn from_files(paths: &[PathBuf]) -> Result<Self> {
        let mut raw = HashMap::new();
        for p in paths {
            let bytes = std::fs::read(p).with_context(|| format!("Cannot read {p:?}"))?;
            raw.insert(p.display().to_string(), bytes);
        }
        Ok(Self { raw })
    }

    pub fn ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self.raw.keys().cloned().collect();
        v.sort();
        v
    }
}

#[async_trait]
impl MailSource for EmlSource {
    fn describe(&self) -> String {
        format!("{} saved mail file(s)", self.raw.len())
    }

    fn checkpoint_key(&self) -> String {
        "eml:files".into()
    }

    fn is_configured(&self) -> bool {
        true
    }

    /// Files are handed to the watcher one by one; there is nothing to list.
    async fn list_changed(&self, _since: DateTime<Utc>, _max: usize) -> Result<Vec<MailHeader>> {
        Ok(Vec::new())
    }

    async fn fetch_mail(&self, id: &str) -> Result<InboundMail> {
        let raw = self.raw.get(id).ok_or_else(|| anyhow::Error::new(MailGone(id.to_string())))?;
        InboundMail::from_rfc822(id, raw)
    }

    async fn mark_processed(&self, _mail_id: &str, _outcome: MailOutcome) -> Result<()> {
        Ok(())
    }

    async fn download_attachment(&self, mail_id: &str, attachment_id: &str, dest: &Path) -> Result<PathBuf> {
        let raw = self.raw.get(mail_id).ok_or_else(|| anyhow::Error::new(MailGone(mail_id.to_string())))?;
        let bytes = rfc822_part(raw, attachment_id)?;
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("Cannot create {dir:?}"))?;
        }
        std::fs::write(dest, bytes).with_context(|| format!("Cannot write {dest:?}"))?;
        Ok(dest.to_path_buf())
    }

    async fn reply(&self, _: &str, _: &str, _: &str) -> Result<()> {
        bail!("a saved mail file cannot be replied to")
    }

    async fn health(&self) -> MailHealth {
        MailHealth { ok: true, detail: self.describe() }
    }
}

/// The decoded body of the MIME part at `path` (`"2"`, `"1.3"`): the ids
/// [`InboundMail::from_rfc822`] gives attachments.
pub fn rfc822_part(raw: &[u8], path: &str) -> Result<Vec<u8>> {
    let parsed = parse_mail(raw).context("Failed parsing RFC822 MIME message")?;
    let mut part: &ParsedMail = &parsed;
    for step in path.split('.') {
        let n: usize = step.parse().map_err(|_| anyhow!("bad MIME part path {path:?}"))?;
        if part.subparts.is_empty() && n == 1 {
            // A single-part message: its one part is "1".
            continue;
        }
        part = part
            .subparts
            .get(n.checked_sub(1).ok_or_else(|| anyhow!("bad MIME part path {path:?}"))?)
            .ok_or_else(|| anyhow!("no MIME part {path:?} in this message"))?;
    }
    Ok(part.get_body_raw()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW: &str = concat!(
        "From: Test <t@example.gr>\r\n",
        "Subject: x\r\n",
        "Message-ID: <a@b>\r\n",
        "MIME-Version: 1.0\r\n",
        "Content-Type: multipart/mixed; boundary=\"OUT\"\r\n\r\n",
        "--OUT\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n\r\n",
        "Το βίντεο στο συνημμένο.\r\n",
        "--OUT\r\n",
        "Content-Type: video/mp4; name=\"clip.mp4\"\r\n",
        "Content-Disposition: attachment; filename=\"clip.mp4\"\r\n",
        "Content-Transfer-Encoding: base64\r\n\r\n",
        "AAECAw==\r\n",
        "--OUT--\r\n",
    );

    #[test]
    fn an_attachment_is_read_back_by_the_id_the_parser_gave_it() {
        let mail = InboundMail::from_rfc822("f", RAW.as_bytes()).unwrap();
        assert_eq!(mail.attachments[0].id, "2");
        assert_eq!(rfc822_part(RAW.as_bytes(), "2").unwrap(), vec![0, 1, 2, 3]);
        assert!(rfc822_part(RAW.as_bytes(), "7").is_err());
        assert!(rfc822_part(RAW.as_bytes(), "x").is_err());
    }
}
