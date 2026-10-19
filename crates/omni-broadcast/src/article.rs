//! Articles with several videos (station policy, product owner 2026-09-25).
//!
//! When the sniffer finds more than one video behind a submitted link:
//!
//! * other **video-platform** posts (X, YouTube, Facebook, …) are queued
//!   automatically as sibling jobs — `1A`, `1B`, `1C` — up to
//!   [`MAX_AUTO_SIBLINGS`];
//! * anything else (raw streams from the page itself, and platform posts
//!   beyond the cap) is **offered** to MCR on the job, one click to queue.
//!
//! Platform posts are what a journalist means when an article embeds them;
//! raw streams on a news page are as often an ad or a "related story" reel,
//! which is why a person decides those.
//!
//! Not every platform post on the page is the article's (P3.14, owner
//! 2026-10-17): one found outside the article's own content (a sidebar, a
//! "more videos" widget, the navigation), a glomex contextual playlist, and
//! one known to run longer than [`MAX_SIBLING_SECS`] are **offered**, not
//! queued. Neither limit applies to the job being processed or to a link the
//! journalist sent, only to the article's other videos.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::downloader::is_video_platform;

/// More than this and the article is a listicle: the rest are offered, not
/// all poured into Dalet.
pub const MAX_AUTO_SIBLINGS: usize = 5;

/// At most this many offers are kept on a job.
pub const MAX_OFFERED: usize = 10;

/// An article's other video longer than this (seconds) is offered, not queued.
/// Exactly this long still queues.
pub const MAX_SIBLING_SECS: f64 = 600.0;

/// What the sniffer and a probe know about one video of the page (P3.14).
/// All unknown by default, which plans as before.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct VideoFacts {
    /// `Some(false)`: found outside the article's own content.
    pub in_article: Option<bool>,
    /// A glomex playlist the player fills from the page.
    pub contextual: bool,
    /// Found only as a plain link, not an embedded player (see `StreamMeta::link`).
    pub link: bool,
    /// Length in seconds when a probe could tell.
    pub duration_secs: Option<f64>,
    /// A live stream: no length, and never ends.
    pub live: bool,
}

impl VideoFacts {
    /// Why this video is not the article's own (a contextual playlist, found
    /// outside its content). Holds even when the journalist asked for a count.
    pub fn placement_hold(&self) -> Option<&'static str> {
        if self.contextual {
            Some("a contextual playlist, not a video the article names")
        } else if self.in_article == Some(false) {
            Some("found outside the article's own content")
        } else if self.link && self.in_article != Some(true) {
            Some("a link outside the article body, not an embedded video")
        } else {
            None
        }
    }

    /// Why this video is offered to MCR instead of queued, if it is.
    pub fn hold_reason(&self) -> Option<&'static str> {
        if let Some(why) = self.placement_hold() {
            Some(why)
        } else if self.live {
            Some("a live stream")
        } else if self.duration_secs.is_some_and(|d| d > MAX_SIBLING_SECS) {
            Some("longer than 10 minutes")
        } else {
            None
        }
    }
}

/// Facts by [`facts_key`] of the video's address.
pub type Facts = HashMap<String, VideoFacts>;

/// One video under two addresses is one video: the key facts are stored under.
pub fn facts_key(url: &str) -> String {
    omni_core::urlnorm::normalize(url)
}

/// `streams` without the videos already queued as siblings of this job
/// (compared as dedup keys). A job that falls back to the sniffer after its
/// first attempt must not take one of them as its own video: it would be
/// delivered twice.
pub fn without_queued(streams: &[String], queued: &[String]) -> Vec<String> {
    let keys: Vec<String> = queued.iter().map(|u| facts_key(u)).collect();
    streams.iter().filter(|u| !keys.contains(&facts_key(u))).cloned().collect()
}

/// The placement facts the sniffer gave, as `(address, in_article, contextual)`.
pub fn facts_from_placement<'a>(placed: impl IntoIterator<Item = (&'a str, Option<bool>, bool)>) -> Facts {
    let mut facts = Facts::new();
    for (u, in_article, contextual) in placed {
        facts
            .entry(facts_key(u))
            .and_modify(|f| {
                // One video under two addresses, seen in two places: inside the
                // article wins, and it is contextual only if every sighting was.
                f.in_article = match (f.in_article, in_article) {
                    (Some(true), _) | (_, Some(true)) => Some(true),
                    (Some(false), Some(false)) => Some(false),
                    _ => None,
                };
                f.contextual &= contextual;
            })
            .or_insert(VideoFacts { in_article, contextual, ..Default::default() });
    }
    facts
}

/// What a probe said about one video's length.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Probed {
    pub duration_secs: Option<f64>,
    pub live: bool,
}

/// Ask `probe` about each video that would be queued as a sibling, and note
/// the answers in `facts`. A sibling the answer holds back is replaced by the
/// next one in line, which is asked in turn, so the probes are at most
/// [`MAX_AUTO_SIBLINGS`] plus the videos held. An unknown answer (`None`)
/// leaves the video queued. Not asked at all when the journalist gave a count
/// (they chose those videos).
pub async fn probe_siblings<F, Fut>(
    page: &str,
    primary: &str,
    all: &[String],
    index: &str,
    max_videos: Option<usize>,
    facts: &mut Facts,
    mut probe: F,
) where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Option<Probed>>,
{
    if max_videos.is_some_and(|n| n > 0) {
        return;
    }
    let mut asked: Vec<String> = Vec::new();
    // Each round asks one new video, so this ends after the candidates run out.
    for _ in 0..all.len() {
        let plan = plan_article_videos_facts(page, primary, all, index, None, facts);
        let Some((url, _)) = plan.siblings.into_iter().find(|(u, _)| !asked.contains(&facts_key(u))) else {
            break;
        };
        asked.push(facts_key(&url));
        if let Some(p) = probe(url.clone()).await {
            let f = facts.entry(facts_key(&url)).or_default();
            f.duration_secs = p.duration_secs;
            f.live = p.live;
        }
    }
}

/// A video found in the article and left for MCR to queue
/// (`queue.candidates_json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Offered {
    pub url: String,
    /// Index the job gets if queued, continuing the article's sequence.
    pub index_str: String,
    /// Set once MCR has queued it, so it cannot be queued twice.
    #[serde(default)]
    pub queued_job_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ArticlePlan {
    /// Index for the job being processed (`1` becomes `1A` once it has
    /// siblings; unchanged otherwise).
    pub primary_index: String,
    /// `(url, index_str)` to queue now.
    pub siblings: Vec<(String, String)>,
    pub offered: Vec<Offered>,
}

/// Index suffixes: `A, B, C, …` after a plain number; `2, 3, …` after an index
/// that already has a letter (`3B` → `3B2`), so nothing collides.
fn suffixer(index: &str) -> (bool, impl Fn(usize) -> String + '_) {
    let numeric = !index.is_empty() && index.chars().all(|c| c.is_ascii_digit());
    let f = move |n: usize| {
        if numeric {
            let mut s = String::new();
            let mut k = n;
            loop {
                s.insert(0, (b'A' + (k % 26) as u8) as char);
                if k < 26 {
                    break;
                }
                k = k / 26 - 1;
            }
            format!("{index}{s}")
        } else {
            format!("{index}{}", n + 1)
        }
    };
    (numeric, f)
}

/// `page` is the link the sniffer opened. Only an article has "other videos":
/// on a platform's own post page the other posts the sniffer sees are replies,
/// the thread above it and "more from this account" — none of them what the
/// journalist sent. Queueing them delivered one X video four times, under four
/// names, for an article that held three different ones.
pub fn plan_article_videos(page: &str, primary: &str, all: &[String], index: &str) -> ArticlePlan {
    plan_article_videos_limited(page, primary, all, index, None)
}

/// [`plan_article_videos_facts`] without any facts.
pub fn plan_article_videos_limited(
    page: &str,
    primary: &str,
    all: &[String],
    index: &str,
    max_videos: Option<usize>,
) -> ArticlePlan {
    plan_article_videos_facts(page, primary, all, index, max_videos, &Facts::new())
}

/// [`plan_article_videos`] for a journalist who asked for only the first
/// `max_videos` ("ΓΙΑ ΠΛΑΝΑ: 2 ΠΡΩΤΑ ΒΙΝΤΕΟ", plan P4.33): this job is the
/// first, the next `max_videos - 1` in page order are queued whatever
/// kind they are (the journalist chose them), and the rest are offered to
/// MCR rather than dropped, in case the count was off.
///
/// `facts` says which of `all` are outside the article, contextual, live or
/// long (see [`VideoFacts::hold_reason`]); those are offered rather than
/// queued. A count never overrides where a video sits (owner 2026-10-17): it
/// is filled only from videos that are not outside the article or a
/// contextual playlist, and those are offered. Length and live do not apply
/// to a count: the journalist chose those videos.
pub fn plan_article_videos_facts(
    page: &str,
    primary: &str,
    all: &[String],
    index: &str,
    max_videos: Option<usize>,
    facts: &Facts,
) -> ArticlePlan {
    let facts_of = |u: &str| facts.get(&facts_key(u)).copied().unwrap_or_default();
    if is_video_platform(page) {
        return ArticlePlan { primary_index: index.to_string(), siblings: Vec::new(), offered: Vec::new() };
    }
    // One video under two addresses is one video: the page embeds
    // "x.com/visegrad24/status/N" and links "x.com/i/status/N" (2026-10-07,
    // the primary was queued again as a sibling). Compared as dedup keys.
    let key = |u: &str| omni_core::urlnorm::normalize(u);
    let mut seen = vec![key(primary)];
    let mut others: Vec<String> = Vec::new();
    for u in all {
        if !seen.contains(&key(u)) {
            seen.push(key(u));
            others.push(u.clone());
        }
    }
    let (numeric, name) = suffixer(index);

    if let Some(max) = max_videos {
        // Stable: page order within each group; the page's own fill the count.
        let (own, foreign): (Vec<String>, Vec<String>) =
            others.into_iter().partition(|u| facts_of(u).placement_hold().is_none());
        let others: Vec<String> = own.into_iter().chain(foreign).collect();
        let own_count = others.iter().take_while(|u| facts_of(u).placement_hold().is_none()).count();
        let wanted = (max.max(1) - 1).min(own_count);
        let mut next = 1usize;
        let mut siblings = Vec::new();
        let mut offered = Vec::new();
        for (i, url) in others.into_iter().enumerate() {
            if i < wanted {
                siblings.push((url, name(next)));
            } else if offered.len() < MAX_OFFERED {
                offered.push(Offered { url, index_str: name(next), queued_job_id: None });
            } else {
                break;
            }
            next += 1;
        }
        let primary_index = if !siblings.is_empty() && numeric { name(0) } else { index.to_string() };
        return ArticlePlan { primary_index, siblings, offered };
    }

    let (platform, raw): (Vec<String>, Vec<String>) = others.into_iter().partition(|u| is_video_platform(u));

    // Position 0 is the primary when it is renamed; siblings follow.
    let mut next = 1usize;
    let mut siblings = Vec::new();
    let mut overflow = Vec::new();
    let mut held = Vec::new();
    for u in platform {
        if facts_of(&u).hold_reason().is_some() {
            held.push(u);
        } else if siblings.len() < MAX_AUTO_SIBLINGS {
            siblings.push((u, name(next)));
            next += 1;
        } else {
            overflow.push(u);
        }
    }
    let offered = overflow
        .into_iter()
        .chain(held)
        .chain(raw)
        .take(MAX_OFFERED)
        .map(|url| {
            let o = Offered { url, index_str: name(next), queued_job_id: None };
            next += 1;
            o
        })
        .collect();

    let primary_index = if !siblings.is_empty() && numeric { name(0) } else { index.to_string() };
    ArticlePlan { primary_index, siblings, offered }
}

/// The slug for a job at `index`, when the original followed the house
/// `{index}_{JOURNALIST}_{KEYWORD}` pattern; `None` for a hand-set slug,
/// which is left alone.
pub fn house_slug(current_slug: &str, current_index: &str, journalist: &str, keyword: &str, index: &str) -> Option<String> {
    (current_slug == format!("{current_index}_{journalist}_{keyword}"))
        .then(|| format!("{index}_{journalist}_{keyword}"))
}

/// The timeline line for one sibling. A retry of the article job plans the
/// same siblings again; dedup stops the second job, and the line must not
/// claim one was queued.
fn sibling_event(url: &str, index: &str, r: &omni_core::models::Enqueued) -> String {
    use omni_core::models::Enqueued;
    match r {
        Enqueued::Created { id } => format!("Another video in this article queued as {index} (job #{id}): {url}"),
        Enqueued::DuplicateActive { existing_id } | Enqueued::DuplicateRecent { existing_id } => {
            format!("Another video in this article is already job #{existing_id}: {url}")
        }
    }
}

/// The article behind a job held more than one video (policy C above): queue the other platform posts as sibling jobs
/// (those `facts` do not hold back),
/// offer the rest to MCR, and rename this job `1` → `1A` when it has siblings.
///
/// Best effort: a failure here is logged and never stops this job's own video.
pub fn queue_article_siblings(
    repo: &omni_core::repository::Repository,
    owner: &str,
    job: &mut omni_core::models::Job,
    primary: &str,
    all_streams: &[String],
    facts: &Facts,
) {
    // A job MCR cancelled (or another worker took) while its page was being
    // resolved must not put the article's other videos on air (plan P7.20).
    match repo.owns_lease(job.id, owner) {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!("Job #{}: no longer ours; not queuing the article's other videos", job.id);
            return;
        }
        Err(e) => {
            tracing::warn!("Job #{}: lease check failed, not queuing the article's other videos: {e:#}", job.id);
            return;
        }
    }
    let max = job.max_videos.filter(|n| *n > 0).map(|n| n as usize);
    let plan = plan_article_videos_facts(&job.url, primary, all_streams, &job.index_str, max, facts);
    if let Some(n) = max {
        let found = 1 + plan.siblings.len() + plan.offered.len();
        if found > n {
            let line = format!(
                "The journalist asked for the first {n} video(s); {found} found. The other {} are offered to MCR.",
                found - n
            );
            tracing::info!("Job #{}: {line}", job.id);
            let _ = repo.record_event(job.id, "INFO", Some(omni_core::models::JobStage::Extract), &line);
        }
    }
    if plan.siblings.is_empty() && plan.offered.is_empty() {
        return;
    }
    let job_id = job.id;

    if plan.primary_index != job.index_str {
        if let Some(slug) = house_slug(&job.slug, &job.index_str, &job.journalist, &job.keyword, &plan.primary_index) {
            match repo.rename_leased_job(job_id, owner, &plan.primary_index, &slug) {
                Ok(true) => {
                    job.index_str = plan.primary_index.clone();
                    job.slug = slug;
                }
                Ok(false) => tracing::warn!("Job #{job_id}: lease lost before the rename to {}", plan.primary_index),
                Err(e) => tracing::warn!("Job #{job_id}: rename failed: {e:#}"),
            }
        }
    }

    for (url, index) in &plan.siblings {
        let mut sibling = omni_core::models::NewJob::new(
            url.clone(),
            format!("{index}_{}_{}", job.journalist, job.keyword),
            job.journalist.clone(),
        );
        sibling.keyword = job.keyword.clone();
        sibling.index_str = index.clone();
        sibling.priority = job.priority;
        sibling.email_source = job.email_source.clone();
        sibling.email_message_id = job.email_message_id.clone();
        sibling.submitted_by_user_id = job.submitted_by_user_id;
        sibling.extraction_method = Some("sniffer".into());
        sibling.group_code = job.group_code.clone();
        sibling.parent_job_id = Some(job_id);
        sibling.notes = Some(format!("Also in the article of job #{job_id}: {}", job.url));
        match repo.enqueue(&sibling, omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS) {
            Ok(r) => {
                let line = sibling_event(url, index, &r);
                // The log gets host and path only; the job's timeline keeps the address.
                tracing::info!("Job #{job_id}: {}", sibling_event(&omni_core::cookies::host_and_path(url), index, &r));
                let _ = repo.record_event(job_id, "INFO", Some(omni_core::models::JobStage::Extract), &line);
            }
            Err(e) => tracing::warn!("Job #{job_id}: could not queue article video {}: {e:#}", omni_core::cookies::host_and_path(url)),
        }
    }

    for o in &plan.offered {
        if let Some(why) = facts.get(&facts_key(&o.url)).and_then(VideoFacts::hold_reason) {
            let line = format!("Another video in this article is offered, not queued ({why}): {}", o.url);
            tracing::info!(
                "Job #{job_id}: Another video in this article is offered, not queued ({why}): {}",
                omni_core::cookies::host_and_path(&o.url)
            );
            let _ = repo.record_event(job_id, "INFO", Some(omni_core::models::JobStage::Extract), &line);
        }
    }

    if !plan.offered.is_empty() {
        match serde_json::to_string(&plan.offered) {
            Ok(json) => {
                let _ = repo.set_candidates(job_id, Some(&json));
                let _ = repo.record_event(
                    job_id,
                    "INFO",
                    Some(omni_core::models::JobStage::Extract),
                    &format!("{} more video(s) in this article offered to MCR", plan.offered.len()),
                );
            }
            Err(e) => tracing::warn!("Job #{job_id}: could not store offered videos: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    const X1: &str = "https://x.com/i/status/1";
    const X2: &str = "https://x.com/i/status/2";
    const X3: &str = "https://x.com/i/status/3";
    const ARTICLE: &str = "https://www.portal.gr/article/1";

    #[test]
    fn three_platform_videos_become_1a_1b_1c() {
        // The newsbomb.gr case: three X posts in one article.
        let p = plan_article_videos(ARTICLE, X1, &s(&[X1, X2, X3]), "1");
        assert_eq!(p.primary_index, "1A");
        assert_eq!(p.siblings, vec![(X2.to_string(), "1B".to_string()), (X3.to_string(), "1C".to_string())]);
        assert!(p.offered.is_empty());
    }

    #[test]
    fn the_primary_under_another_address_is_not_queued_again() {
        // news247, 2026-10-07: the embed said ".../visegrad24/status/N", a
        // link on the page ".../i/status/N"; the second became a sibling job
        // downloading the primary's own post again.
        let embed = "https://x.com/visegrad24/status/1";
        let p = plan_article_videos(ARTICLE, embed, &s(&[embed, X2, X1]), "1");
        assert_eq!(p.siblings, vec![(X2.to_string(), "1B".to_string())]);
        // ...and two addresses of one sibling are one sibling.
        let p = plan_article_videos(ARTICLE, X1, &s(&[X1, X2, "https://twitter.com/someone/status/2?s=20"]), "1");
        assert_eq!(p.siblings.len(), 1, "{:?}", p.siblings);
    }

    #[test]
    fn raw_streams_are_offered_not_queued() {
        let raw = "https://cdn.portal.gr/video/master.m3u8";
        let p = plan_article_videos(ARTICLE, X1, &s(&[X1, raw, X2]), "2");
        assert_eq!(p.primary_index, "2A");
        assert_eq!(p.siblings, vec![(X2.to_string(), "2B".to_string())]);
        assert_eq!(p.offered, vec![Offered { url: raw.into(), index_str: "2C".into(), queued_job_id: None }]);
    }

    #[test]
    fn a_single_video_changes_nothing() {
        let p = plan_article_videos(ARTICLE, X1, &s(&[X1]), "1");
        assert_eq!(p.primary_index, "1");
        assert!(p.siblings.is_empty() && p.offered.is_empty());
        // Only raw extras: offered, and the primary keeps its plain index.
        let p = plan_article_videos(ARTICLE, X1, &s(&[X1, "https://cdn.portal.gr/a.mp4"]), "1");
        assert_eq!(p.primary_index, "1");
        assert_eq!(p.offered[0].index_str, "1B");
    }

    #[test]
    fn a_listicle_is_capped_and_the_rest_offered() {
        let many: Vec<String> = (1..=9).map(|i| format!("https://x.com/i/status/{i}")).collect();
        let p = plan_article_videos(ARTICLE, &many[0], &many, "1");
        assert_eq!(p.siblings.len(), MAX_AUTO_SIBLINGS);
        assert_eq!(p.offered.len(), 9 - 1 - MAX_AUTO_SIBLINGS);
        assert_eq!(p.siblings.last().unwrap().1, "1F");
        assert_eq!(p.offered[0].index_str, "1G");
    }

    #[test]
    fn an_index_that_already_has_a_letter_gets_numbers() {
        let p = plan_article_videos(ARTICLE, X1, &s(&[X1, X2]), "3B");
        assert_eq!(p.primary_index, "3B");
        assert_eq!(p.siblings[0].1, "3B2");
    }

    #[test]
    fn duplicates_and_the_primary_itself_are_not_repeated() {
        let p = plan_article_videos(ARTICLE, X1, &s(&[X1, X2, X2, X1]), "1");
        assert_eq!(p.siblings.len(), 1);
    }

    #[test]
    fn a_post_page_is_not_an_article() {
        // Sniffing an X post whose yt-dlp fetch failed: the page also shows
        // the post itself under its author's handle and two replies. None of
        // them is another video of the submission (trial run 2026-09-25).
        let page = "https://x.com/i/status/1900000000000000010";
        let seen = s(&[
            "https://x.com/NewsDeskOne/status/1900000000000000010",
            "https://x.com/SomeReader/status/1900000000000000011",
            "https://x.com/OtherReader/status/1900000000000000012",
        ]);
        let p = plan_article_videos(page, "https://video.twimg.com/amplify_video/1/pl/master.m3u8", &seen, "1B");
        assert_eq!(p.primary_index, "1B");
        assert!(p.siblings.is_empty(), "{:?}", p.siblings);
        assert!(p.offered.is_empty(), "{:?}", p.offered);
    }

    #[test]
    fn a_sibling_already_queued_is_not_reported_as_queued_again() {
        use omni_core::models::Enqueued;
        let fresh = sibling_event(X2, "1B", &Enqueued::Created { id: 7 });
        assert_eq!(fresh, format!("Another video in this article queued as 1B (job #7): {X2}"));
        let again = sibling_event(X2, "1A2", &Enqueued::DuplicateActive { existing_id: 7 });
        assert!(!again.contains("queued as") && again.contains("job #7"), "{again}");
    }

    /// iefimerida.gr, 2026-10-05: "ΓΙΑ ΠΛΑΝΑ: 2 ΠΡΩΤΑ ΒΙΝΤΕΟ" on an article with
    /// four Streamable videos. All four were delivered.
    #[test]
    fn the_first_n_asked_for_are_queued_and_the_rest_offered() {
        let v: Vec<String> = (1..=4).map(|i| format!("https://streamable.com/v{i}")).collect();
        let p = plan_article_videos_limited(ARTICLE, &v[0], &v, "8", Some(2));
        assert_eq!(p.primary_index, "8A");
        assert_eq!(p.siblings, vec![(v[1].clone(), "8B".to_string())]);
        let offered: Vec<(&str, &str)> = p.offered.iter().map(|o| (o.url.as_str(), o.index_str.as_str())).collect();
        assert_eq!(offered, vec![(v[2].as_str(), "8C"), (v[3].as_str(), "8D")]);

        // Only the first: the job keeps its plain number, the rest offered.
        let p = plan_article_videos_limited(ARTICLE, &v[0], &v, "8", Some(1));
        assert_eq!(p.primary_index, "8");
        assert!(p.siblings.is_empty());
        assert_eq!(p.offered.len(), 3);

        // A raw stream the journalist counted is queued, not just offered.
        let raw = "https://cdn.portal.gr/video/master.m3u8";
        let p = plan_article_videos_limited(ARTICLE, X1, &s(&[X1, raw, X2]), "2", Some(2));
        assert_eq!(p.siblings, vec![(raw.to_string(), "2B".to_string())]);

        // More asked for than found: everything is queued, nothing offered.
        let p = plan_article_videos_limited(ARTICLE, X1, &s(&[X1, X2]), "1", Some(5));
        assert_eq!(p.siblings.len(), 1);
        assert!(p.offered.is_empty());
    }

    fn facts(v: &[(&str, VideoFacts)]) -> Facts {
        v.iter().map(|(u, f)| (facts_key(u), *f)).collect()
    }

    fn plan_with(f: &Facts, all: &[&str]) -> ArticlePlan {
        plan_article_videos_facts(ARTICLE, all[0], &s(all), "1", None, f)
    }

    #[test]
    fn a_sibling_longer_than_ten_minutes_is_offered_exactly_ten_is_queued() {
        let long = VideoFacts { duration_secs: Some(601.0), ..Default::default() };
        let edge = VideoFacts { duration_secs: Some(600.0), ..Default::default() };
        let p = plan_with(&facts(&[(X2, long), (X3, edge)]), &[X1, X2, X3]);
        assert_eq!(p.siblings, vec![(X3.to_string(), "1B".to_string())], "{p:?}");
        assert_eq!(p.offered, vec![Offered { url: X2.into(), index_str: "1C".into(), queued_job_id: None }]);
        assert_eq!(p.primary_index, "1A");
    }

    #[test]
    fn an_unknown_duration_queues_as_before_and_a_live_stream_is_offered() {
        let p = plan_with(&facts(&[(X2, VideoFacts::default())]), &[X1, X2]);
        assert_eq!(p.siblings.len(), 1);
        let live = VideoFacts { live: true, ..Default::default() };
        let p = plan_with(&facts(&[(X2, live)]), &[X1, X2]);
        assert!(p.siblings.is_empty() && p.offered.len() == 1, "{p:?}");
    }

    #[test]
    fn a_sibling_outside_the_article_or_a_contextual_playlist_is_offered() {
        let outside = VideoFacts { in_article: Some(false), ..Default::default() };
        let inside = VideoFacts { in_article: Some(true), ..Default::default() };
        let ctx = VideoFacts { in_article: Some(true), contextual: true, ..Default::default() };
        let glomex = "https://player.glomex.com/integration/1/iframe-player.html?playlistId=cl-abc";
        let p = plan_with(&facts(&[(X2, outside), (X3, inside), (glomex, ctx)]), &[X1, X2, X3, glomex]);
        assert_eq!(p.siblings, vec![(X3.to_string(), "1B".to_string())], "{p:?}");
        let offered: Vec<&str> = p.offered.iter().map(|o| o.url.as_str()).collect();
        assert_eq!(offered, vec![X2, glomex]);
        // Unknown placement is not "outside": queued as before. A glomex
        // player is not a platform post and is only ever offered.
        let p = plan_with(&Facts::new(), &[X1, X2, glomex]);
        assert_eq!(p.siblings, vec![(X2.to_string(), "1B".to_string())]);
        assert_eq!(p.offered.len(), 1);
    }

    #[test]
    fn facts_match_one_video_under_another_address() {
        let long = VideoFacts { duration_secs: Some(4200.0), ..Default::default() };
        let p = plan_with(&facts(&[("https://twitter.com/someone/status/2?s=20", long)]), &[X1, X2]);
        assert!(p.siblings.is_empty() && p.offered.len() == 1, "{p:?}");
    }

    #[test]
    fn a_count_is_filled_only_from_the_articles_own_videos() {
        let outside = VideoFacts { in_article: Some(false), ..Default::default() };
        let long = VideoFacts { duration_secs: Some(4200.0), ..Default::default() };
        // The widget's video is listed second but is not one of "the first 2";
        // a long video the journalist counted is still queued.
        let f = facts(&[(X2, outside), (X3, long)]);
        let p = plan_article_videos_facts(ARTICLE, X1, &s(&[X1, X2, X3]), "1", Some(2), &f);
        assert_eq!(p.siblings, vec![(X3.to_string(), "1B".to_string())], "{p:?}");
        assert_eq!(p.offered.iter().map(|o| o.url.as_str()).collect::<Vec<_>>(), vec![X2]);

        // The waiter page with a count of 2: the contextual playlist and the
        // header-navigation show are offered, never queued, though the page
        // has no other video to fill the count with.
        let glomex = "https://player.glomex.com/integration/1/iframe-player.html?playlistId=cl-abc";
        let ctx = VideoFacts { contextual: true, ..Default::default() };
        let f = facts(&[(glomex, ctx), (X2, outside)]);
        let p = plan_article_videos_facts(ARTICLE, X1, &s(&[X1, glomex, X2]), "1", Some(2), &f);
        assert!(p.siblings.is_empty(), "{p:?}");
        assert_eq!(p.primary_index, "1");
        assert_eq!(p.offered.iter().map(|o| o.url.as_str()).collect::<Vec<_>>(), vec![glomex, X2]);

        // A count larger than the page's own videos is not made up from the others.
        let p = plan_article_videos_facts(ARTICLE, X1, &s(&[X1, X3, X2]), "1", Some(3), &facts(&[(X2, outside)]));
        assert_eq!(p.siblings, vec![(X3.to_string(), "1B".to_string())], "{p:?}");
        assert_eq!(p.offered.len(), 1);
    }

    #[test]
    fn placement_facts_are_found_by_the_address_the_planner_uses() {
        let f = facts_from_placement([("https://twitter.com/someone/status/2?s=20", Some(false), false), (X3, Some(true), false)]);
        let p = plan_with(&f, &[X1, X2, X3]);
        assert_eq!(p.siblings, vec![(X3.to_string(), "1B".to_string())], "{p:?}");
        assert_eq!(p.offered[0].url, X2);
        // The control: facts under a key that is not this video's change nothing.
        let wrong = facts_from_placement([("https://x.com/i/status/999", Some(false), false)]);
        assert_eq!(plan_with(&wrong, &[X1, X2, X3]).siblings.len(), 2);
    }

    #[tokio::test]
    async fn a_held_sibling_is_replaced_by_the_next_and_that_one_is_probed_too() {
        let urls: Vec<String> = (1..=8).map(|i| format!("https://x.com/i/status/{i}")).collect();
        let long = urls[1].clone();
        let asked = std::cell::RefCell::new(Vec::<String>::new());
        let mut f = Facts::new();
        probe_siblings(ARTICLE, &urls[0], &urls, "1", None, &mut f, |u| {
            asked.borrow_mut().push(u.clone());
            let answer = if u == long { Probed { duration_secs: Some(4200.0), live: false } } else { Probed { duration_secs: Some(30.0), live: false } };
            async move { Some(answer) }
        })
        .await;
        // 5 siblings at first (2..=6); #2 is long, so #7 is promoted and asked: 6 probes.
        assert_eq!(asked.borrow().len(), MAX_AUTO_SIBLINGS + 1, "{:?}", asked.borrow());
        assert!(asked.borrow().contains(&urls[6]));
        assert!(!asked.borrow().contains(&urls[7]), "#8 stays an overflow offer, unasked");
        let p = plan_article_videos_facts(ARTICLE, &urls[0], &urls, "1", None, &f);
        assert!(p.siblings.iter().all(|(u, _)| *u != long) && p.siblings.len() == MAX_AUTO_SIBLINGS, "{p:?}");
    }

    #[tokio::test]
    async fn nothing_is_probed_for_a_count_and_an_unknown_answer_keeps_the_video() {
        let urls = s(&[X1, X2, X3]);
        let mut f = Facts::new();
        let n = std::cell::Cell::new(0);
        probe_siblings(ARTICLE, X1, &urls, "1", Some(2), &mut f, |_| {
            n.set(n.get() + 1);
            async { None }
        })
        .await;
        assert_eq!(n.get(), 0);
        probe_siblings(ARTICLE, X1, &urls, "1", None, &mut f, |_| {
            n.set(n.get() + 1);
            async { None }
        })
        .await;
        assert_eq!(n.get(), 2);
        assert_eq!(plan_article_videos_facts(ARTICLE, X1, &urls, "1", None, &f).siblings.len(), 2);
    }

    #[test]
    fn two_sightings_of_one_video_merge_placement() {
        let f = facts_from_placement([
            ("https://twitter.com/someone/status/2?s=20", Some(false), true),
            (X2, Some(true), false),
        ]);
        let v = f[&facts_key(X2)];
        assert_eq!((v.in_article, v.contextual), (Some(true), false));
        let f = facts_from_placement([(X2, Some(false), true), (X2, Some(false), true)]);
        let v = f[&facts_key(X2)];
        assert_eq!((v.in_article, v.contextual), (Some(false), true));
        let f = facts_from_placement([(X2, Some(false), false), (X2, None, false)]);
        assert_eq!(f[&facts_key(X2)].in_article, None);
    }

    #[test]
    fn a_link_outside_the_article_body_is_offered_one_inside_is_queued() {
        let mut f = Facts::new();
        f.insert(facts_key(X2), VideoFacts { link: true, in_article: None, ..Default::default() });
        f.insert(facts_key(X3), VideoFacts { link: true, in_article: Some(true), ..Default::default() });
        let p = plan_with(&f, &[X1, X2, X3]);
        assert_eq!(p.siblings, vec![(X3.to_string(), "1B".to_string())], "{p:?}");
        assert_eq!(p.offered[0].url, X2);
    }


    #[test]
    fn a_video_already_queued_as_a_sibling_is_not_taken_again() {
        let left = without_queued(&s(&[X2, X3]), &s(&["https://twitter.com/someone/status/2?s=20"]));
        assert_eq!(left, vec![X3.to_string()]);
        assert_eq!(without_queued(&s(&[X2]), &[]), vec![X2.to_string()]);
        assert!(without_queued(&s(&[X2]), &s(&[X2])).is_empty());
    }


    #[test]
    fn only_house_slugs_are_renamed() {
        assert_eq!(house_slug("1_MCR_ASDC", "1", "MCR", "ASDC", "1A").as_deref(), Some("1A_MCR_ASDC"));
        assert_eq!(house_slug("CUSTOM_NAME", "1", "MCR", "ASDC", "1A"), None);
    }
}
