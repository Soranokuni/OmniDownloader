//! IMAP [`MailSource`] (plan P4.1): kept for on-prem servers and tests.
//!
//! Basic authentication only, which Exchange Online no longer accepts
//! (defect E-01) — Graph is the default for Office 365.
//!
//! The `imap` crate is blocking, so every call runs its whole session inside
//! one `spawn_blocking` and returns plain data. Nothing inside re-enters the
//! runtime (defect E-05 was a `Handle::block_on` in exactly that position).

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};

use crate::mail::InboundMail;
use crate::source::{MailHealth, MailOutcome, MailSource};

#[derive(Clone)]
pub struct ImapMailSource {
    server: String,
    port: u16,
    username: String,
    password: String,
}

impl std::fmt::Debug for ImapMailSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The password never reaches a log line, not even through `{:?}`.
        f.debug_struct("ImapMailSource")
            .field("server", &self.server)
            .field("port", &self.port)
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

type Session = imap::Session<Box<dyn imap::ImapConnection>>;

impl ImapMailSource {
    pub fn new(server: &str, port: u16, username: &str, password: &str) -> Self {
        Self {
            server: server.trim().to_string(),
            port,
            username: username.trim().to_string(),
            password: password.to_string(),
        }
    }

    fn session(&self) -> Result<Session> {
        let client = imap::ClientBuilder::new(self.server.as_str(), self.port)
            .connect()
            .with_context(|| format!("Failed to connect to IMAP {}:{}", self.server, self.port))?;
        let mut session = client
            .login(&self.username, &self.password)
            .map_err(|(e, _)| anyhow!("IMAP login failed for {}: {}", self.username, e))?;
        session.select("INBOX").map_err(|e| anyhow!("Select INBOX failed: {e}"))?;
        Ok(session)
    }

    /// Run a blocking IMAP session off the async runtime.
    async fn with_session<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Session) -> Result<T> + Send + 'static,
    {
        let me = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut session = me.session()?;
            let out = f(&mut session);
            let _ = session.logout();
            out
        })
        .await
        .map_err(|e| anyhow!("IMAP task failed: {e}"))?
    }

    /// Connect, log in, log out. For the admin "test connection" button.
    pub fn test_connection_blocking(&self) -> Result<()> {
        let mut session = self.session()?;
        let _ = session.logout();
        Ok(())
    }

    fn fetch_raw(session: &mut Session, uid: &str) -> Result<Vec<u8>> {
        // BODY.PEEK[]: a plain RFC822 / BODY[] fetch sets \Seen as a side
        // effect, which marked mail read before it was processed (E-02).
        let fetches = session
            .uid_fetch(uid, "(UID BODY.PEEK[])")
            .map_err(|e| anyhow!("IMAP fetch of UID {uid} failed: {e}"))?;
        let msg = fetches.iter().next().ok_or_else(|| anyhow!("UID {uid} not found"))?;
        Ok(msg.body().ok_or_else(|| anyhow!("UID {uid} has no body"))?.to_vec())
    }
}

#[async_trait]
impl MailSource for ImapMailSource {
    fn describe(&self) -> String {
        format!("imap {}", self.server)
    }

    fn is_configured(&self) -> bool {
        !self.server.is_empty() && !self.username.is_empty() && !self.password.is_empty()
    }

    async fn fetch_unprocessed(&self, limit: usize) -> Result<Vec<InboundMail>> {
        self.with_session(move |session| {
            let mut uids: Vec<u32> = session
                // UNFLAGGED: a message given up on is flagged and left unread
                // for a human; it must not be picked up again every poll.
                .uid_search("UNSEEN UNFLAGGED")
                .map_err(|e| anyhow!("IMAP search UNSEEN failed: {e}"))?
                .into_iter()
                .collect();
            uids.sort_unstable(); // UIDs ascend with arrival: oldest first
            let mut out = Vec::new();
            for uid in uids.into_iter().take(limit) {
                let id = uid.to_string();
                match Self::fetch_raw(session, &id).and_then(|raw| InboundMail::from_rfc822(&id, &raw)) {
                    Ok(mail) => out.push(mail),
                    Err(e) => tracing::warn!("IMAP: skipping UID {uid} this poll: {e:#}"),
                }
            }
            Ok(out)
        })
        .await
    }

    async fn mark_processed(&self, mail_id: &str, outcome: MailOutcome) -> Result<()> {
        let uid = mail_id.to_string();
        self.with_session(move |session| {
            // IMAP has no portable "move to folder"; a failed message stays
            // unread and gets a flag, so it stands out in any mail client.
            let flags = match outcome {
                MailOutcome::Processed => "+FLAGS (\\Seen)",
                MailOutcome::Failed => "+FLAGS (\\Flagged)",
            };
            session
                .uid_store(&uid, flags)
                .map_err(|e| anyhow!("IMAP store on UID {uid} failed: {e}"))?;
            Ok(())
        })
        .await
    }

    async fn download_attachment(&self, mail_id: &str, attachment_id: &str, dest: &Path) -> Result<PathBuf> {
        let uid = mail_id.to_string();
        let raw = self.with_session(move |session| Self::fetch_raw(session, &uid)).await?;
        let parsed = mailparse::parse_mail(&raw).context("Failed parsing RFC822 MIME message")?;
        let mut part = &parsed;
        for step in attachment_id.split('.') {
            let i: usize = step.parse().with_context(|| format!("bad attachment id {attachment_id}"))?;
            part = part
                .subparts
                .get(i.wrapping_sub(1))
                .ok_or_else(|| anyhow!("attachment {attachment_id} not found"))?;
        }
        let bytes = part.get_body_raw().context("decoding attachment")?;
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(dest, bytes).await?;
        Ok(dest.to_path_buf())
    }

    async fn reply(&self, _mail_id: &str, _html: &str, _text: &str) -> Result<()> {
        bail!("replies are sent through Microsoft Graph; IMAP cannot send mail")
    }

    async fn health(&self) -> MailHealth {
        if !self.is_configured() {
            return MailHealth {
                ok: false,
                detail: "IMAP not configured".into(),
            };
        }
        let me = self.clone();
        match tokio::task::spawn_blocking(move || me.test_connection_blocking()).await {
            Ok(Ok(())) => MailHealth {
                ok: true,
                detail: format!("logged in to {}", self.server),
            },
            Ok(Err(e)) => MailHealth {
                ok: false,
                detail: e.to_string().chars().take(160).collect(),
            },
            Err(_) => MailHealth {
                ok: false,
                detail: "IMAP health task failed".into(),
            },
        }
    }
}
