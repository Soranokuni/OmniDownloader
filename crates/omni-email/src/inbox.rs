//! The MCR mail view's list (plan P7.7): one entry per handled mail and per
//! link added by hand, newest first, each with the state an operator
//! filters by. Pure; the repository supplies the rows.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use omni_core::models::{InboxJobRow, InboxMailRow, JobStatus};
use omni_core::translit::translit;

use crate::mail::AttachmentMeta;
use crate::mail_view::{how_el, index_order, preview, MailParseSummary};
use crate::watcher::QueuedFromMail;

/// How long the list's one-line preview may be.
pub const PREVIEW_CHARS: usize = 180;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    Mail,
    /// A link added in the MCR form or a journalist's own page.
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryState {
    /// The mail could not be read; nothing was queued.
    Failed,
    /// A video needs a person: review, a manual download, a failure.
    Attention,
    /// Waiting or being worked on.
    Active,
    /// Everything delivered (or discarded).
    Done,
    /// Nothing to download: photos only, no links.
    Empty,
}

/// The filter chips of the list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InboxFilter {
    #[default]
    All,
    Active,
    Attention,
    Done,
    Empty,
}

impl InboxFilter {
    pub fn admits(self, state: EntryState) -> bool {
        match self {
            Self::All => true,
            Self::Active => state == EntryState::Active,
            Self::Attention => matches!(state, EntryState::Attention | EntryState::Failed),
            Self::Done => state == EntryState::Done,
            Self::Empty => state == EntryState::Empty,
        }
    }
}

/// A job, as the list's progress bar needs it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EntryJob {
    pub id: i64,
    pub index_str: String,
    pub status: String,
    pub stage: String,
    pub progress: f64,
    pub parent_job_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub kind: EntryKind,
    /// The Message-ID of a mail; the job id of a manual entry.
    pub key: String,
    /// When the mail arrived, or the link was added.
    pub at: Option<DateTime<Utc>>,
    pub subject: String,
    pub from_name: Option<String>,
    pub from_address: Option<String>,
    pub journalist: Option<String>,
    /// How the journalist was found, in Greek.
    pub how: Option<String>,
    pub group_code: Option<String>,
    pub urgent: bool,
    /// `JOBS`, `PHOTOS_ONLY`, `NO_LINKS` or `FAILED`; `None` for manual.
    pub outcome: Option<String>,
    pub state: EntryState,
    pub preview: String,
    pub attachments: usize,
    /// Who added a manual entry (a user id; `None` for an MCR desk without
    /// a sign-in).
    pub added_by: Option<i64>,
    pub jobs: Vec<EntryJob>,
    /// Lowercase text the search looks in, in both scripts.
    #[serde(skip)]
    haystack: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct InboxCounts {
    pub all: usize,
    pub active: usize,
    pub attention: usize,
    pub done: usize,
    pub empty: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct InboxPage {
    pub entries: Vec<Entry>,
    pub total: usize,
    pub page: usize,
    pub per_page: usize,
    /// Per filter chip, for what the search matches.
    pub counts: InboxCounts,
}

/// The jobs the mails' duplicate links point at: they may be older than the
/// list's window, so the repository is asked for them by id.
pub fn referenced_job_ids(mails: &[InboxMailRow]) -> Vec<i64> {
    let mut ids: Vec<i64> = mails
        .iter()
        .flat_map(|m| serde_json::from_str::<Vec<QueuedFromMail>>(&m.jobs_json).unwrap_or_default())
        .map(|q| q.result.job_id())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// The list, newest first: every mail in `mails`, and every job in `jobs`
/// added by hand since `since` (older ones are there only because a mail's
/// duplicate link points at them).
pub fn entries(mails: Vec<InboxMailRow>, jobs: Vec<InboxJobRow>, since: DateTime<Utc>) -> Vec<Entry> {
    let by_id: HashMap<i64, &InboxJobRow> = jobs.iter().map(|j| (j.id, j)).collect();
    let mut by_mail: HashMap<&str, Vec<&InboxJobRow>> = HashMap::new();
    let mut children: HashMap<i64, Vec<&InboxJobRow>> = HashMap::new();
    for j in &jobs {
        if let Some(k) = j.email_message_id.as_deref() {
            by_mail.entry(k).or_default().push(j);
        }
        if let Some(p) = j.parent_job_id.filter(|p| *p != j.id) {
            children.entry(p).or_default().push(j);
        }
    }
    for list in children.values_mut().chain(by_mail.values_mut()) {
        list.sort_by(|a, b| index_order(&a.index_str, &b.index_str).then(a.id.cmp(&b.id)));
    }

    let mut out = Vec::with_capacity(mails.len());
    for m in &mails {
        let queued: Vec<QueuedFromMail> = serde_json::from_str(&m.jobs_json).unwrap_or_default();
        let mut seen = HashSet::new();
        let mut js: Vec<&InboxJobRow> = Vec::new();
        for q in &queued {
            gather(q.result.job_id(), &by_id, &children, &mut seen, &mut js);
        }
        for j in by_mail.get(m.internet_message_id.as_str()).into_iter().flatten() {
            gather(j.id, &by_id, &children, &mut seen, &mut js);
        }
        let summary: Option<MailParseSummary> = m.parse_json.as_deref().and_then(|p| serde_json::from_str(p).ok());
        let subject = m.subject.clone().unwrap_or_default();
        let preview = m.body_head.as_deref().map(|b| preview(b, &subject, PREVIEW_CHARS)).unwrap_or_default();
        let attachments = m
            .attachments_json
            .as_deref()
            .and_then(|a| serde_json::from_str::<Vec<AttachmentMeta>>(a).ok())
            .map(|v| v.len())
            .unwrap_or(0);
        let state = if m.outcome == "FAILED" { EntryState::Failed } else { state_of(&js) };
        let journalist = summary
            .as_ref()
            .map(|s| s.journalist.clone())
            .or_else(|| js.first().map(|j| j.journalist.clone()));
        let haystack = haystack(
            [Some(subject.as_str()), m.from_name.as_deref(), m.from_address.as_deref(), journalist.as_deref(), Some(preview.as_str())]
                .into_iter()
                .flatten()
                .chain(js.iter().flat_map(|j| [j.slug.as_str(), j.url.as_str()])),
        );
        out.push(Entry {
            kind: EntryKind::Mail,
            key: m.internet_message_id.clone(),
            at: m.received_at.or(m.processed_at),
            subject,
            from_name: m.from_name.clone().filter(|n| !n.trim().is_empty()),
            from_address: m.from_address.clone(),
            how: summary.as_ref().map(|s| how_el(s.how).to_string()),
            group_code: summary
                .as_ref()
                .and_then(|s| s.group.as_ref().map(|g| g.code.clone()))
                .or_else(|| js.iter().find_map(|j| j.group_code.clone())),
            urgent: summary.as_ref().is_some_and(|s| s.urgent),
            outcome: Some(m.outcome.clone()),
            state,
            preview,
            attachments,
            added_by: None,
            jobs: js.iter().map(|j| entry_job(j)).collect(),
            journalist,
            haystack,
        });
    }

    let added_by_hand = |j: &&InboxJobRow| {
        j.email_message_id.is_none() && j.parent_job_id.is_none() && j.created_at.is_some_and(|at| at >= since)
    };
    for j in jobs.iter().filter(added_by_hand) {
        let mut seen = HashSet::new();
        let mut js: Vec<&InboxJobRow> = Vec::new();
        gather(j.id, &by_id, &children, &mut seen, &mut js);
        out.push(Entry {
            kind: EntryKind::Manual,
            key: j.id.to_string(),
            at: j.created_at,
            subject: short_url(&j.url),
            from_name: None,
            from_address: None,
            journalist: Some(j.journalist.clone()),
            how: None,
            group_code: j.group_code.clone(),
            urgent: false,
            outcome: None,
            state: state_of(&js),
            preview: j.url.clone(),
            attachments: 0,
            added_by: j.submitted_by_user_id,
            haystack: haystack(
                [j.journalist.as_str()].into_iter().chain(js.iter().flat_map(|x| [x.slug.as_str(), x.url.as_str()])),
            ),
            jobs: js.iter().map(|x| entry_job(x)).collect(),
        });
    }

    out.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| b.key.cmp(&a.key)));
    out
}

/// Where a recent entry's videos stand, for the desk's chime (plan P5.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Settlement {
    /// [`Entry::id`].
    pub id: String,
    pub subject: String,
    pub journalist: Option<String>,
    /// Every video has reached an end state: nothing waits or runs.
    pub settled: bool,
    /// Settled with every video delivered (or discarded by MCR).
    pub ok: bool,
    pub delivered: usize,
    pub total: usize,
}

/// The entries received at or after `since` that have videos, each with
/// whether all of them have ended. The desk compares two answers and chimes
/// for an entry that went from unsettled to settled: "this email is done".
/// Settled is not [`EntryState::Done`]: a mail with one video in review and
/// another still downloading is Attention, and not finished yet.
pub fn settlements(entries: &[Entry], since: DateTime<Utc>) -> Vec<Settlement> {
    use JobStatus::*;
    entries
        .iter()
        .filter(|e| !e.jobs.is_empty() && e.at.is_some_and(|at| at >= since))
        .map(|e| {
            let status = |j: &EntryJob| JobStatus::parse(&j.status);
            Settlement {
                id: e.id(),
                subject: e.subject.clone(),
                journalist: e.journalist.clone(),
                settled: !e.jobs.iter().any(|j| matches!(status(j), Some(Pending | Running))),
                ok: e.state == EntryState::Done,
                delivered: e.jobs.iter().filter(|j| matches!(status(j), Some(Completed | CompletedManual))).count(),
                total: e.jobs.len(),
            }
        })
        .collect()
}

impl Entry {
    /// `mail:<Message-ID>` or `manual:<job id>`: what the desk calls it.
    pub fn id(&self) -> String {
        match self.kind {
            EntryKind::Mail => format!("mail:{}", self.key),
            EntryKind::Manual => format!("manual:{}", self.key),
        }
    }
}

/// One page of `entries` for a filter chip and a search.
pub fn page(entries: Vec<Entry>, filter: InboxFilter, search: &str, page: usize, per_page: usize) -> InboxPage {
    page_with_focus(entries, filter, search, page, per_page, None)
}

/// [`page`], but the page that holds the entry `focus` (an [`Entry::id`])
/// when it is in the list: the desk opening a mail from a video.
pub fn page_with_focus(
    entries: Vec<Entry>,
    filter: InboxFilter,
    search: &str,
    page: usize,
    per_page: usize,
    focus: Option<&str>,
) -> InboxPage {
    let q = search.trim().to_lowercase();
    let q_latin = translit(search.trim()).to_lowercase();
    let matching: Vec<Entry> = if q.is_empty() {
        entries
    } else {
        entries.into_iter().filter(|e| e.haystack.contains(&q) || e.haystack.contains(&q_latin)).collect()
    };

    let mut counts = InboxCounts { all: matching.len(), ..Default::default() };
    for e in &matching {
        match e.state {
            EntryState::Active => counts.active += 1,
            EntryState::Attention | EntryState::Failed => counts.attention += 1,
            EntryState::Done => counts.done += 1,
            EntryState::Empty => counts.empty += 1,
        }
    }

    let filtered: Vec<Entry> = matching.into_iter().filter(|e| filter.admits(e.state)).collect();
    let per_page = per_page.clamp(5, 100);
    let total = filtered.len();
    let pages = total.div_ceil(per_page).max(1);
    let page = match focus.and_then(|id| filtered.iter().position(|e| e.id() == id)) {
        Some(at) => at / per_page + 1,
        None => page.clamp(1, pages),
    };
    InboxPage {
        entries: filtered.into_iter().skip((page - 1) * per_page).take(per_page).collect(),
        total,
        page,
        per_page,
        counts,
    }
}

/// A job and the videos found in its article, once each.
fn gather<'a>(
    root: i64,
    by_id: &HashMap<i64, &'a InboxJobRow>,
    children: &HashMap<i64, Vec<&'a InboxJobRow>>,
    seen: &mut HashSet<i64>,
    out: &mut Vec<&'a InboxJobRow>,
) {
    if let Some(j) = by_id.get(&root) {
        if seen.insert(root) {
            out.push(j);
        }
        for c in children.get(&root).into_iter().flatten() {
            if seen.insert(c.id) {
                out.push(c);
            }
        }
    }
}

fn entry_job(j: &InboxJobRow) -> EntryJob {
    EntryJob {
        id: j.id,
        index_str: j.index_str.clone(),
        status: j.status.clone(),
        stage: j.stage.clone(),
        progress: j.progress,
        parent_job_id: j.parent_job_id,
    }
}

fn state_of(jobs: &[&InboxJobRow]) -> EntryState {
    use JobStatus::*;
    let status = |j: &&InboxJobRow| JobStatus::parse(&j.status);
    // An unreadable status is shown as needing a person, never as done.
    if jobs.iter().any(|j| matches!(status(j), Some(RequiresReview | ManualDownload | Failed) | None)) {
        EntryState::Attention
    } else if jobs.iter().any(|j| matches!(status(j), Some(Pending | Running))) {
        EntryState::Active
    } else if jobs.is_empty() {
        EntryState::Empty
    } else {
        EntryState::Done
    }
}

/// `www.portal.gr/kosmos/…/arthro`: enough of a link to recognise it.
fn short_url(url: &str) -> String {
    let lower = url.to_ascii_lowercase();
    let cut = ["https://www.", "http://www.", "https://", "http://"]
        .iter()
        .find(|p| lower.starts_with(*p))
        .map(|p| p.len())
        .unwrap_or(0);
    let rest = &url[cut..];
    if rest.chars().count() <= 70 {
        return rest.to_string();
    }
    let head: String = rest.chars().take(67).collect();
    format!("{head}…")
}

/// What the search looks in: the text lowercased, and its ELOT 743 Latin
/// form, so `seismos` finds «Σεισμός» and «σεισμος» finds «ΣΕΙΣΜΟΣ».
fn haystack<'a>(parts: impl Iterator<Item = &'a str>) -> String {
    let joined = parts.collect::<Vec<_>>().join(" ");
    format!("{} {}", joined.to_lowercase(), translit(&joined).to_lowercase())
}
