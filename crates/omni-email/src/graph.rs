//! Microsoft Graph [`MailSource`] (plan P4.2, defect E-01).
//!
//! OAuth2 client credentials against the station tenant, then plain REST.
//! Fully async on reqwest — no blocking client, no `spawn_blocking` (E-05).
//!
//! The station's app holds `Mail.Read` only (plan P4.8), so by default the
//! mailbox is never written: nothing is marked read or moved, and the
//! database alone records what was handled. With `graph.write_access` (and
//! `Mail.ReadWrite`) the mailbox also shows it:
//! * processed mail is marked read and moved to `Omni/Processed`;
//! * mail that could not be processed is moved to `Omni/Failed` and left
//!   **unread**, so a human sees it (E-02);
//! * folders are created on first use.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::{Method, RequestBuilder, Response, StatusCode, Url};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tracing::{info, warn};

use omni_core::config::GraphConfig;

use crate::mail::{AttachmentMeta, InboundMail};
use crate::source::{MailGone, MailHeader, MailHealth, MailOutcome, MailSource};

/// Headers per listing page. Graph allows up to 1000 for messages; a small
/// page keeps each response quick, and `$select` keeps each entry tiny.
const LIST_PAGE: usize = 100;

/// What a full fetch selects. The listing selects [`HEADER_FIELDS`] only.
const MESSAGE_FIELDS: &str =
    "id,internetMessageId,subject,from,toRecipients,ccRecipients,receivedDateTime,lastModifiedDateTime,body,hasAttachments";
const HEADER_FIELDS: &str = "id,internetMessageId,subject,from,receivedDateTime,lastModifiedDateTime";

pub const LOGIN_BASE: &str = "https://login.microsoftonline.com";
pub const GRAPH_BASE: &str = "https://graph.microsoft.com";

/// Refresh this long before the token says it expires.
const TOKEN_MARGIN: Duration = Duration::from_secs(120);

/// The server asked us to wait (HTTP 429, or 503 with `Retry-After`).
///
/// Carried in the anyhow chain so the poll loop can honour it instead of
/// hammering a throttled tenant.
#[derive(Debug, Clone, Copy)]
pub struct RetryAfter(pub Duration);

impl std::fmt::Display for RetryAfter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Graph asked to retry after {}s", self.0.as_secs())
    }
}

impl std::error::Error for RetryAfter {}

/// Graph answered with an error status. `message` is Graph's own
/// `error.message`, which names the problem and carries no credential.
#[derive(Debug, Clone)]
pub struct GraphError {
    pub status: StatusCode,
    pub message: String,
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Graph returned {}: {}", self.status, self.message)
    }
}

impl std::error::Error for GraphError {}

fn graph_status(e: &anyhow::Error) -> Option<StatusCode> {
    e.chain().find_map(|c| c.downcast_ref::<GraphError>()).map(|g| g.status)
}

struct Token {
    value: String,
    valid_until: Instant,
}

pub struct GraphMailSource {
    http: reqwest::Client,
    cfg: GraphConfig,
    login_base: String,
    graph_base: String,
    token: Mutex<Option<Token>>,
    /// Folder path ("Omni/Processed") → Graph folder id, resolved once.
    folders: Mutex<HashMap<String, String>>,
}

impl std::fmt::Debug for GraphMailSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No secret, no token: this can end up in a log line.
        f.debug_struct("GraphMailSource")
            .field("tenant_id", &self.cfg.tenant_id)
            .field("client_id", &self.cfg.client_id)
            .field("mailbox", &self.cfg.mailbox)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default = "default_expires_in")]
    expires_in: u64,
}

fn default_expires_in() -> u64 {
    3600
}

#[derive(Deserialize)]
struct GList<T> {
    value: Vec<T>,
    #[serde(default, rename = "@odata.nextLink")]
    next_link: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct GAddress {
    #[serde(default)]
    name: String,
    #[serde(default)]
    address: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct GRecipient {
    #[serde(default)]
    email_address: GAddress,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct GBody {
    #[serde(default)]
    content_type: String,
    #[serde(default)]
    content: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GMessage {
    id: String,
    #[serde(default)]
    internet_message_id: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    from: Option<GRecipient>,
    #[serde(default)]
    to_recipients: Vec<GRecipient>,
    #[serde(default)]
    cc_recipients: Vec<GRecipient>,
    #[serde(default)]
    received_date_time: Option<DateTime<Utc>>,
    #[serde(default)]
    last_modified_date_time: Option<DateTime<Utc>>,
    #[serde(default)]
    body: Option<GBody>,
    #[serde(default)]
    has_attachments: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GAttachment {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    is_inline: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GFolder {
    id: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    unread_item_count: Option<i64>,
}

impl GraphMailSource {
    pub fn new(cfg: GraphConfig) -> Self {
        Self::with_endpoints(cfg, LOGIN_BASE, GRAPH_BASE)
    }

    /// Point at other endpoints: a national cloud, or a mock server in tests.
    pub fn with_endpoints(cfg: GraphConfig, login_base: &str, graph_base: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self {
            http,
            cfg,
            login_base: login_base.trim_end_matches('/').to_string(),
            graph_base: graph_base.trim_end_matches('/').to_string(),
            token: Mutex::new(None),
            folders: Mutex::new(HashMap::new()),
        }
    }

    /// `{graph}/v1.0/users/{mailbox}/{segments...}`, each segment escaped.
    fn url(&self, segments: &[&str]) -> Result<Url> {
        let mut url = Url::parse(&self.graph_base).context("bad Graph base URL")?;
        {
            let mut p = url.path_segments_mut().map_err(|_| anyhow!("bad Graph base URL"))?;
            p.pop_if_empty().push("v1.0").push("users").push(self.cfg.mailbox.trim());
            for s in segments {
                p.push(s);
            }
        }
        Ok(url)
    }

    async fn token(&self, force_refresh: bool) -> Result<String> {
        let mut guard = self.token.lock().await;
        if !force_refresh {
            if let Some(t) = guard.as_ref() {
                if Instant::now() < t.valid_until {
                    return Ok(t.value.clone());
                }
            }
        }
        let url = format!(
            "{}/{}/oauth2/v2.0/token",
            self.login_base,
            urlencode(self.cfg.tenant_id.trim())
        );
        let resp = self
            .http
            .post(&url)
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", self.cfg.client_id.trim()),
                ("client_secret", self.cfg.client_secret.as_str()),
                ("scope", "https://graph.microsoft.com/.default"),
            ])
            .send()
            .await
            .context("cannot reach login.microsoftonline.com")?;
        let status = resp.status();
        if !status.is_success() {
            // The body is an AAD error ("AADSTS7000215: Invalid client secret
            // provided"); it names the problem and carries no secret.
            let body = resp.text().await.unwrap_or_default();
            let code = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("error_description").and_then(|d| d.as_str()).map(String::from))
                .unwrap_or(body);
            let first_line = code.lines().next().unwrap_or("").chars().take(200).collect::<String>();
            bail!("Graph login failed ({status}): {first_line}");
        }
        let t: TokenResponse = resp.json().await.context("unreadable token response")?;
        let life = Duration::from_secs(t.expires_in).saturating_sub(TOKEN_MARGIN);
        *guard = Some(Token {
            value: t.access_token.clone(),
            valid_until: Instant::now() + life,
        });
        Ok(t.access_token)
    }

    /// Send with a bearer token; on 401 refresh the token once and retry.
    /// 429 and 503-with-Retry-After become a [`RetryAfter`] error.
    async fn send(&self, build: impl Fn(&reqwest::Client) -> RequestBuilder) -> Result<Response> {
        let mut refreshed = false;
        loop {
            let token = self.token(refreshed).await?;
            let resp = build(&self.http).bearer_auth(&token).send().await.context("cannot reach Graph")?;
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED && !refreshed {
                refreshed = true;
                continue;
            }
            if status == StatusCode::TOO_MANY_REQUESTS
                || (status == StatusCode::SERVICE_UNAVAILABLE && resp.headers().contains_key("retry-after"))
            {
                let secs = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .unwrap_or(30);
                return Err(anyhow::Error::new(RetryAfter(Duration::from_secs(secs.clamp(1, 600)))));
            }
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                let msg = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v.pointer("/error/message").and_then(|m| m.as_str()).map(String::from))
                    .unwrap_or(body);
                return Err(anyhow::Error::new(GraphError {
                    status,
                    message: msg.chars().take(200).collect(),
                }));
            }
            return Ok(resp);
        }
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: Url) -> Result<T> {
        let resp = self.send(|c| c.get(url.clone())).await?;
        resp.json().await.context("unreadable Graph response")
    }

    async fn send_json(&self, method: Method, url: Url, body: serde_json::Value) -> Result<Response> {
        self.send(|c| c.request(method.clone(), url.clone()).json(&body)).await
    }

    /// Folder id for a path like `Omni/Processed`, creating what is missing.
    async fn folder_id(&self, path: &str) -> Result<String> {
        if let Some(id) = self.folders.lock().await.get(path) {
            return Ok(id.clone());
        }
        let mut parent: Option<String> = None;
        for name in path.split('/').map(str::trim).filter(|s| !s.is_empty()) {
            let list_url = match &parent {
                None => self.url(&["mailFolders"])?,
                Some(p) => self.url(&["mailFolders", p, "childFolders"])?,
            };
            let mut filtered = list_url.clone();
            filtered
                .query_pairs_mut()
                .append_pair("$filter", &format!("displayName eq '{}'", name.replace('\'', "''")))
                .append_pair("$select", "id,displayName");
            let found: GList<GFolder> = self.get_json(filtered).await?;
            let id = match found.value.into_iter().find(|f| f.display_name.eq_ignore_ascii_case(name)) {
                Some(f) => f.id,
                None => {
                    info!("Graph: creating mail folder '{name}' in {}", self.cfg.mailbox);
                    let created: GFolder = self
                        .send_json(Method::POST, list_url, serde_json::json!({ "displayName": name }))
                        .await?
                        .json()
                        .await
                        .context("unreadable folder-creation response")?;
                    created.id
                }
            };
            parent = Some(id);
        }
        let id = parent.ok_or_else(|| anyhow!("empty folder path"))?;
        self.folders.lock().await.insert(path.to_string(), id.clone());
        Ok(id)
    }

    async fn attachments(&self, message_id: &str) -> Result<Vec<AttachmentMeta>> {
        let mut url = self.url(&["messages", message_id, "attachments"])?;
        url.query_pairs_mut().append_pair("$select", "id,name,contentType,size,isInline");
        let list: GList<GAttachment> = self.get_json(url).await?;
        Ok(list
            .value
            .into_iter()
            // Inline parts are signature logos and pasted screenshots.
            .filter(|a| !a.is_inline)
            .map(|a| AttachmentMeta {
                id: a.id,
                name: a.name,
                content_type: a.content_type.unwrap_or_default().to_ascii_lowercase(),
                size: a.size,
            })
            .collect())
    }

    fn to_inbound(&self, m: GMessage, attachments: Vec<AttachmentMeta>) -> InboundMail {
        let from = m.from.unwrap_or_default().email_address;
        let body = m.body.unwrap_or_default();
        let (body_text, body_html) = if body.content_type.eq_ignore_ascii_case("html") {
            (String::new(), Some(body.content))
        } else {
            (body.content, None)
        };
        InboundMail {
            id: m.id,
            internet_message_id: m.internet_message_id.unwrap_or_default(),
            from_address: from.address.trim().to_lowercase(),
            from_name: from.name,
            to: m.to_recipients.into_iter().map(|r| r.email_address.address.to_lowercase()).collect(),
            cc: m.cc_recipients.into_iter().map(|r| r.email_address.address.to_lowercase()).collect(),
            subject: m.subject.unwrap_or_default(),
            received_at: m.received_date_time,
            body_text,
            body_html,
            attachments,
        }
    }
}

impl GraphMailSource {
    /// A `@odata.nextLink` is followed only on the Graph host we were given:
    /// the bearer token goes with it.
    fn same_origin(&self, link: &str) -> Result<Url> {
        let url = Url::parse(link).context("unreadable @odata.nextLink")?;
        let base = Url::parse(&self.graph_base).context("bad Graph base URL")?;
        if url.origin() != base.origin() {
            bail!("Graph paging link points at another host ({}); not followed", url.origin().ascii_serialization());
        }
        Ok(url)
    }

    /// The admin panel's "Test mailbox" (plan P4.7): sign in, then read the
    /// inbox folder and one message id. Reading the folder alone would not
    /// prove the app may read messages. Each failure says what to fix.
    pub async fn test_access(&self) -> Result<String> {
        if !self.is_configured() {
            bail!("Fill in tenant id, client id, mailbox and the client secret first.");
        }
        self.token(true).await?;
        let explain = |e: anyhow::Error| match graph_status(&e) {
            Some(StatusCode::FORBIDDEN) | Some(StatusCode::UNAUTHORIZED) => anyhow!(
                "Signed in, but the app may not read {}: grant Mail.Read (application) with admin                  consent, and check the application access policy covers this mailbox.",
                self.cfg.mailbox
            ),
            Some(StatusCode::NOT_FOUND) => anyhow!("Signed in, but there is no mailbox {} in this tenant.", self.cfg.mailbox),
            _ => e,
        };
        let mut inbox_url = self.url(&["mailFolders", "Inbox"])?;
        inbox_url.query_pairs_mut().append_pair("$select", "id,displayName,unreadItemCount");
        let inbox: GFolder = self.get_json(inbox_url).await.map_err(explain)?;
        let mut one = self.url(&["mailFolders", "Inbox", "messages"])?;
        one.query_pairs_mut().append_pair("$top", "1").append_pair("$select", "id");
        let _: GList<serde_json::Value> = self.get_json(one).await.map_err(explain)?;
        Ok(format!(
            "Signed in; the Inbox of {} is readable ({} unread).",
            self.cfg.mailbox,
            inbox.unread_item_count.unwrap_or(0)
        ))
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[async_trait]
impl MailSource for GraphMailSource {
    fn describe(&self) -> String {
        format!("graph {}", self.cfg.mailbox)
    }

    fn is_configured(&self) -> bool {
        self.cfg.is_configured()
    }

    fn checkpoint_key(&self) -> String {
        format!("graph:{}", self.cfg.mailbox.trim().to_lowercase())
    }

    async fn list_changed(&self, since: DateTime<Utc>, max: usize) -> Result<Vec<MailHeader>> {
        let mut url = self.url(&["mailFolders", "Inbox", "messages"])?;
        url.query_pairs_mut()
            // Not `isRead`: with Mail.Read nothing can be marked read, and a
            // person opening the mailbox in Outlook would hide mail from us.
            // lastModifiedDateTime also moves when a mail is moved *into*
            // the inbox (rescued from Junk), which receivedDateTime does not.
            // Graph wants the $orderby property in $filter, and it is.
            .append_pair(
                "$filter",
                &format!("lastModifiedDateTime ge {}", since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            )
            .append_pair("$orderby", "lastModifiedDateTime asc")
            .append_pair("$top", &LIST_PAGE.min(max.max(1)).to_string())
            .append_pair("$select", HEADER_FIELDS);
        let mut out = Vec::new();
        let mut next = Some(url);
        while let Some(page_url) = next.take() {
            let page: GList<GMessage> = self.get_json(page_url).await?;
            for m in page.value {
                // Graph always sends it; a message without one cannot be
                // placed against the checkpoint, so it is not listed.
                let Some(modified_at) = m.last_modified_date_time else {
                    warn!("Graph: message {} has no lastModifiedDateTime; skipped", m.id);
                    continue;
                };
                out.push(MailHeader {
                    internet_message_id: m.internet_message_id.unwrap_or_default(),
                    subject: m.subject.unwrap_or_default(),
                    from_address: m.from.unwrap_or_default().email_address.address.trim().to_lowercase(),
                    received_at: m.received_date_time,
                    modified_at,
                    id: m.id,
                });
            }
            if out.len() >= max {
                out.truncate(max);
                break;
            }
            next = match page.next_link {
                Some(link) => Some(self.same_origin(&link)?),
                None => None,
            };
        }
        Ok(out)
    }

    async fn fetch_mail(&self, id: &str) -> Result<InboundMail> {
        let mut url = self.url(&["messages", id])?;
        url.query_pairs_mut().append_pair("$select", MESSAGE_FIELDS);
        // The HTML as sent, not Graph's text rendering: our own converter
        // keeps the address behind a hyperlinked word and is what the
        // golden fixtures test (plan P4.10).
        let m: GMessage = match self.get_json(url).await {
            Ok(m) => m,
            Err(e) if graph_status(&e) == Some(StatusCode::NOT_FOUND) => {
                return Err(anyhow::Error::new(MailGone(id.to_string())));
            }
            Err(e) => return Err(e),
        };
        let attachments = if m.has_attachments {
            // An error here fails the fetch: better to retry the message next
            // poll than to parse it without the video it carries.
            self.attachments(&m.id).await.context("attachments unreadable")?
        } else {
            Vec::new()
        };
        Ok(self.to_inbound(m, attachments))
    }

    async fn mark_processed(&self, mail_id: &str, outcome: MailOutcome) -> Result<()> {
        if !self.cfg.write_access {
            // Mail.Read only: the database is the record (plan P4.8).
            return Ok(());
        }
        let folder = match outcome {
            MailOutcome::Processed => {
                // Read first: if the move fails, a read message is already
                // out of the unread filter and is not fetched again.
                self.send_json(
                    Method::PATCH,
                    self.url(&["messages", mail_id])?,
                    serde_json::json!({ "isRead": true }),
                )
                .await?;
                self.cfg.processed_folder.clone()
            }
            // Left unread: that is what makes a human look at it.
            MailOutcome::Failed => self.cfg.failed_folder.clone(),
        };
        let dest = self.folder_id(&folder).await?;
        self.send_json(
            Method::POST,
            self.url(&["messages", mail_id, "move"])?,
            serde_json::json!({ "destinationId": dest }),
        )
        .await?;
        Ok(())
    }

    async fn download_attachment(&self, mail_id: &str, attachment_id: &str, dest: &Path) -> Result<PathBuf> {
        let url = self.url(&["messages", mail_id, "attachments", attachment_id, "$value"])?;
        let mut resp = self.send(|c| c.get(url.clone())).await?;
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Streamed: a 2 GB rush file must not be held in memory.
        let partial = dest.with_extension("part");
        let mut file = tokio::fs::File::create(&partial).await?;
        while let Some(chunk) = resp.chunk().await.context("attachment download interrupted")? {
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&partial, dest).await?;
        Ok(dest.to_path_buf())
    }

    async fn reply(&self, mail_id: &str, html: &str, _text: &str) -> Result<()> {
        self.send_json(
            Method::POST,
            self.url(&["messages", mail_id, "reply"])?,
            serde_json::json!({ "message": { "body": { "contentType": "HTML", "content": html } } }),
        )
        .await?;
        Ok(())
    }

    async fn health(&self) -> MailHealth {
        if !self.is_configured() {
            return MailHealth {
                ok: false,
                detail: "Graph not configured".into(),
            };
        }
        let probe = async {
            let mut url = self.url(&["mailFolders", "Inbox"])?;
            url.query_pairs_mut().append_pair("$select", "id,displayName,unreadItemCount");
            let inbox: GFolder = self.get_json(url).await?;
            Ok::<_, anyhow::Error>(inbox.unread_item_count.unwrap_or(0))
        };
        match probe.await {
            Ok(unread) => MailHealth {
                ok: true,
                detail: format!("{}: {unread} unread", self.cfg.mailbox),
            },
            Err(e) => MailHealth {
                ok: false,
                detail: e.to_string().chars().take(200).collect(),
            },
        }
    }
}
