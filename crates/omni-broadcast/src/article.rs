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

use serde::{Deserialize, Serialize};

use crate::downloader::is_video_platform;

/// More than this and the article is a listicle: the rest are offered, not
/// all poured into Dalet.
pub const MAX_AUTO_SIBLINGS: usize = 5;

/// At most this many offers are kept on a job.
pub const MAX_OFFERED: usize = 10;

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
    if is_video_platform(page) {
        return ArticlePlan { primary_index: index.to_string(), siblings: Vec::new(), offered: Vec::new() };
    }
    let mut seen = vec![primary.to_string()];
    let mut others = Vec::new();
    for u in all {
        if !seen.contains(u) {
            seen.push(u.clone());
            others.push(u.clone());
        }
    }
    let (platform, raw): (Vec<String>, Vec<String>) = others.into_iter().partition(|u| is_video_platform(u));

    let (numeric, name) = suffixer(index);
    // Position 0 is the primary when it is renamed; siblings follow.
    let mut next = 1usize;
    let mut siblings = Vec::new();
    let mut overflow = Vec::new();
    for u in platform {
        if siblings.len() < MAX_AUTO_SIBLINGS {
            siblings.push((u, name(next)));
            next += 1;
        } else {
            overflow.push(u);
        }
    }
    let offered = overflow
        .into_iter()
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

/// The article behind a job held more than one video (policy C above): queue the other platform posts as sibling jobs,
/// offer the rest to MCR, and rename this job `1` → `1A` when it has siblings.
///
/// Best effort: a failure here is logged and never stops this job's own video.
pub fn queue_article_siblings(
    repo: &omni_core::repository::Repository,
    owner: &str,
    job: &mut omni_core::models::Job,
    primary: &str,
    all_streams: &[String],
) {
    let plan = plan_article_videos(&job.url, primary, all_streams, &job.index_str);
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
        sibling.notes = Some(format!("Also in the article of job #{job_id}: {}", job.url));
        match repo.enqueue(&sibling, omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS) {
            Ok(r) => {
                tracing::info!("Job #{job_id}: article video {url} queued as {index} ({r:?})");
                let _ = repo.record_event(
                    job_id,
                    "INFO",
                    Some(omni_core::models::JobStage::Extract),
                    &format!("Another video in this article queued as {index} (job #{}): {url}", r.job_id()),
                );
            }
            Err(e) => tracing::warn!("Job #{job_id}: could not queue article video {url}: {e:#}"),
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
    fn only_house_slugs_are_renamed() {
        assert_eq!(house_slug("1_MCR_ASDC", "1", "MCR", "ASDC", "1A").as_deref(), Some("1A_MCR_ASDC"));
        assert_eq!(house_slug("CUSTOM_NAME", "1", "MCR", "ASDC", "1A"), None);
    }
}
