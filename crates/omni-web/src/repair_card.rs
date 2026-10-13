//! What the approval card says about a repaired video (plan P10.11b): how it
//! was found, the gate's checklist for the candidate that was held, the length
//! against the page's, and the article-picture evidence. Built from the held
//! repair attempt alone; hosts only, never a full address.

use omni_core::repository::RepairAttempt;
use serde_json::{json, Value};

/// `found` for the card: `shipped` | `recipe` | `llm` | `other`.
/// `origin` is the recipe's origin (`shipped` | `admin` | `llm`), `approvals`
/// how many times MCR approved it on the page's domain. A recipe the language
/// model wrote that MCR never approved is still `llm`; once approved it is a
/// known recipe like any other.
pub fn found_kind(rung: Option<&str>, recipe_id: Option<i64>, origin: Option<&str>, approvals: i64) -> &'static str {
    if rung == Some("llm") {
        return "llm";
    }
    match (recipe_id, origin) {
        (None, _) => "other",
        (Some(_), Some("shipped")) => "shipped",
        (Some(_), Some("llm")) if approvals == 0 => "llm",
        (Some(_), _) => "recipe",
    }
}

/// The card's `repair` object. `None` for an attempt that is not a held one.
pub fn build(attempt: &RepairAttempt, origin: Option<&str>, approvals: i64) -> Option<Value> {
    if attempt.outcome.as_deref() != Some("held") {
        return None;
    }
    let list: Vec<Value> = attempt.candidates_json.as_deref().and_then(|t| serde_json::from_str(t).ok()).unwrap_or_default();
    let held = usize::try_from(attempt.next_index).ok().and_then(|i| list.get(i));
    let checks: Vec<Value> = held
        .and_then(|c| c.get("checks"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|c| {
                    json!({
                        "name": c.get("name").and_then(Value::as_str).unwrap_or(""),
                        "ok": c.get("ok").and_then(Value::as_bool).unwrap_or(false),
                        "detail": c.get("detail").and_then(Value::as_str).unwrap_or(""),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let short_clip = checks.iter().any(|c| c["name"] == "short" && c["ok"] == false);
    let source_host = attempt.held_url.as_deref().and_then(|u| omni_core::net::parse_public_url(u).ok()).map(|(_, h)| h);
    let image = attempt.image_host.as_ref().map(|_| {
        json!({
            "distance": attempt.image_distance,
            "frame_secs": attempt.image_frame_secs,
            "match": attempt.image_distance.is_some_and(|d| u32::try_from(d).is_ok_and(omni_broadcast::imagehash::is_match)),
        })
    });
    Some(json!({
        "found": found_kind(attempt.rung.as_deref(), attempt.recipe_id, origin, approvals),
        "recipe_approvals": approvals,
        "source_host": source_host,
        "duration_secs": held.and_then(|c| c.get("duration_secs")).and_then(Value::as_f64),
        "expected_secs": attempt.expected_secs,
        "checks": checks,
        "short_clip": short_clip,
        "image": image,
    }))
}

/// The card's `repair` object with how many verified candidates MCR could still
/// ask for after this one («Επόμενη επιλογή (n)», P10.13).
pub fn with_next_left(mut card: Value, next_left: usize) -> Value {
    card["next_left"] = json!(next_left);
    card
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checks(short_ok: bool) -> Value {
        json!([
            {"name": "egress", "ok": true, "detail": "Δημόσιος κόμβος cdn.news.example της σελίδας"},
            {"name": "probe", "ok": true, "detail": "120 δλ, 720p (cdn.news.example)"},
            {"name": "short", "ok": short_ok, "detail": "Σύντομο — μπορεί να είναι διαφήμιση"},
        ])
    }

    fn attempt() -> RepairAttempt {
        let first = json!({"url": "https://first.example/a.mp4", "duration_secs": 30.0, "checks": [{"name": "probe", "ok": false, "detail": "first"}]});
        let second = json!({"url": "https://cdn.news.example/b.mp4", "duration_secs": 118.5, "checks": checks(true)});
        RepairAttempt {
            id: 1,
            job_id: 7,
            started_at: None,
            finished_at: None,
            mode: "hold".into(),
            rung: Some("recipes".into()),
            outcome: Some("held".into()),
            recipe_hash: Some("abcdef0123".into()),
            recipe_id: Some(3),
            domain: Some("news.example".into()),
            llm_calls: 0,
            tokens_in: 0,
            tokens_out: 0,
            candidates_json: Some(json!([first, second]).to_string()),
            next_index: 1,
            duration_ms: None,
            held_url: Some("https://cdn.news.example/b.mp4?token=SECRET".into()),
            mcr_decision: None,
            image_distance: None,
            image_frame_secs: None,
            image_host: None,
            expected_secs: Some(120.0),
        }
    }

    #[test]
    fn the_card_describes_the_held_candidate_not_the_first_one() {
        let v = build(&attempt(), Some("admin"), 4).unwrap();
        assert_eq!(v["source_host"], "cdn.news.example");
        assert_eq!(v["duration_secs"], 118.5, "the held candidate's own length");
        assert_eq!(v["expected_secs"], 120.0);
        let names: Vec<&str> = v["checks"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["egress", "probe", "short"], "the checks of #2, after a fallback from #1");
        assert!(!v.to_string().contains("SECRET") && !v.to_string().contains("b.mp4"), "hosts only: {v}");
        assert_eq!(v["image"], Value::Null, "no article picture, no image block");
    }

    #[test]
    fn the_card_carries_how_many_next_choices_are_left() {
        let v = with_next_left(build(&attempt(), None, 0).unwrap(), 3);
        assert_eq!(v["next_left"], 3);
        assert_eq!(with_next_left(build(&attempt(), None, 0).unwrap(), 0)["next_left"], 0);
        assert_eq!(build(&attempt(), None, 0).unwrap().get("next_left"), None, "only the route adds it");
    }

    #[test]
    fn found_is_shipped_recipe_llm_or_other() {
        assert_eq!(found_kind(Some("recipes"), Some(1), Some("shipped"), 0), "shipped");
        assert_eq!(found_kind(Some("recipes"), Some(1), Some("admin"), 0), "recipe");
        assert_eq!(found_kind(Some("recipes"), Some(1), Some("admin"), 5), "recipe");
        assert_eq!(found_kind(Some("recipes"), Some(1), Some("llm"), 0), "llm", "a model-written recipe nobody approved yet");
        assert_eq!(found_kind(Some("recipes"), Some(1), Some("llm"), 2), "recipe", "approved twice: a known recipe");
        assert_eq!(found_kind(Some("llm"), None, None, 0), "llm");
        assert_eq!(found_kind(Some("recipes"), None, None, 0), "other");
        assert_eq!(build(&attempt(), Some("admin"), 4).unwrap()["recipe_approvals"], 4);
    }

    #[test]
    fn a_short_clip_is_flagged_only_when_the_gates_short_check_failed() {
        let mut a = attempt();
        assert_eq!(build(&a, None, 0).unwrap()["short_clip"], false, "a failed probe on #1 is not 'short'");
        let c = json!([{"url": "https://x.example/a.mp4", "duration_secs": 9.0, "checks": checks(false)}]);
        a.candidates_json = Some(c.to_string());
        a.next_index = 0;
        assert_eq!(build(&a, None, 0).unwrap()["short_clip"], true);
        let c = json!([{"url": "https://x.example/a.mp4", "duration_secs": 90.0, "checks": checks(true)}]);
        a.candidates_json = Some(c.to_string());
        assert_eq!(build(&a, None, 0).unwrap()["short_clip"], false, "the short check passed");
    }

    #[test]
    fn the_image_block_says_match_no_match_or_not_compared() {
        let mut a = attempt();
        a.image_host = Some("img.news.example".into());
        for (d, want) in [(0, true), (10, true), (11, false), (32, false)] {
            a.image_distance = Some(d);
            a.image_frame_secs = Some(6.0);
            let v = build(&a, None, 0).unwrap();
            assert_eq!(v["image"]["match"], want, "distance {d}");
            assert_eq!(v["image"]["distance"], d);
            assert_eq!(v["image"]["frame_secs"], 6.0);
        }
        // A picture was kept but nothing compared (no preview video): never a match.
        a.image_distance = None;
        a.image_frame_secs = None;
        let v = build(&a, None, 0).unwrap();
        assert_eq!((&v["image"]["match"], &v["image"]["distance"]), (&json!(false), &Value::Null));
        // No picture at all.
        a.image_host = None;
        assert_eq!(build(&a, None, 0).unwrap()["image"], Value::Null);
    }

    #[test]
    fn only_a_held_attempt_has_a_card_and_broken_json_is_empty_not_an_error() {
        let mut a = attempt();
        a.outcome = Some("shadow_found".into());
        assert!(build(&a, None, 0).is_none());
        let mut a = attempt();
        a.candidates_json = Some("not json".into());
        a.next_index = 9;
        let v = build(&a, None, 0).unwrap();
        assert_eq!((v["checks"].as_array().unwrap().len(), &v["duration_secs"]), (0, &Value::Null));
    }
}
