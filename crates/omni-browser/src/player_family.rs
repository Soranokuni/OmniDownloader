//! Which video player a page uses (plan P10.10), by plain signatures in what
//! the snapshot saw. Sites that share a player share the way its video is
//! found, so the recipe store keys shared recipes by this family.

use crate::evidence::PageSnapshot;

/// Families in priority order: the first one with a matching signature wins,
/// so a Brightcove page (which runs video.js) is Brightcove, not videojs.
/// Signatures are lowercase; matching is case-insensitive.
const FAMILIES: &[(&str, &[&str])] = &[
    ("jwplayer", &["cdn.jwplayer.com", "jwplayer.js", "jwplatform", "jwpcdn"]),
    ("brightcove", &["players.brightcove.net"]),
    ("glomex", &["glomex"]),
    ("bitmovin", &["bitmovin"]),
    ("theoplayer", &["theoplayer"]),
    ("kaltura", &["kaltura"]),
    ("shaka", &["shaka-player"]),
    ("flowplayer", &["flowplayer"]),
    ("clappr", &["clappr"]),
    ("plyr", &["plyr"]),
    ("dailymotion", &["dailymotion.com/embed", "geo.dailymotion.com"]),
    ("youtube", &["youtube.com/embed", "youtube-nocookie.com"]),
    ("vimeo", &["player.vimeo.com"]),
    ("videojs", &["video.js", "video.min.js", "class=\"video-js", "vjs-"]),
    ("hlsjs", &["hls.js", "hls.min.js"]),
];

/// `sig` in `hay`. A file name ending in `.js` must end there: `video.js` is not
/// `video.json`.
fn has_signature(hay: &str, sig: &str) -> bool {
    if !sig.ends_with(".js") {
        return hay.contains(sig);
    }
    hay.match_indices(sig).any(|(at, _)| {
        !hay[at + sig.len()..].chars().next().map_or(false, |c| c.is_ascii_alphanumeric())
    })
}

/// The player family of the page, or `None` when no signature matches.
/// Looks at request addresses, inline-script windows, player blocks and the page.
pub fn player_family(snap: &PageSnapshot) -> Option<&'static str> {
    let mut hay = String::new();
    let mut add = |s: &str| {
        hay.push_str(&s.to_ascii_lowercase());
        hay.push('\n');
    };
    for n in &snap.network {
        add(&n.url);
    }
    for s in &snap.scripts {
        add(s);
    }
    for p in &snap.players {
        add(&p.html);
    }
    if let Some(h) = &snap.html {
        add(h);
    }
    FAMILIES
        .iter()
        .find(|(_, sigs)| sigs.iter().any(|s| has_signature(&hay, s)))
        .map(|(name, _)| *name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::{NetworkEntry, PlayerBlock};

    fn page() -> PageSnapshot {
        PageSnapshot::new("https://news.example.gr/a")
    }

    /// Where a signature appears; every kind of evidence must count.
    fn with(sig: &str, place: usize) -> PageSnapshot {
        let mut s = page();
        match place % 4 {
            0 => s.network.push(NetworkEntry { url: format!("https://static.example.gr/{sig}/x.js"), ..Default::default() }),
            1 => s.scripts.push(format!("var a = 1; load('{sig}'); var b = 2;")),
            2 => s.players.push(PlayerBlock { tag: "div".into(), html: format!("<div class=\"{sig}\"></div>"), index: 0, in_article: true }),
            _ => s.html = Some(format!("<html><body><script src=\"/{sig}\"></script></body></html>")),
        }
        s
    }

    #[test]
    fn every_family_is_recognised_from_each_kind_of_evidence() {
        let mut place = 0;
        for (family, sigs) in FAMILIES {
            for sig in *sigs {
                // the signature as the page would show it, in any letter case
                for text in [sig.to_string(), sig.to_ascii_uppercase()] {
                    let snap = with(&text, place);
                    place += 1;
                    assert_eq!(player_family(&snap), Some(*family), "{text} in place {}", (place - 1) % 4);
                }
            }
        }
    }

    #[test]
    fn invented_pages_of_each_family() {
        let table: [(&str, &str); 15] = [
            ("https://cdn.jwplayer.com/players/abc-xyz.js", "jwplayer"),
            ("https://players.brightcove.net/123/default_default/index.min.js", "brightcove"),
            ("https://player.glomex.com/integration/1/glomex-integration.js", "glomex"),
            ("https://cdn.bitmovin.com/player/web/8/bitmovinplayer.js", "bitmovin"),
            ("https://cdn.theoplayer.com/dash/theoplayer/THEOplayer.js", "theoplayer"),
            ("https://cdnapisec.kaltura.com/p/1/embedPlaykitJs/uiconf_id/2", "kaltura"),
            ("https://cdn.example.gr/shaka-player.compiled.js", "shaka"),
            ("https://releases.flowplayer.org/7.2.7/flowplayer.min.js", "flowplayer"),
            ("https://cdn.example.gr/clappr.min.js", "clappr"),
            ("https://cdn.plyr.io/3.7.8/plyr.js", "plyr"),
            ("https://geo.dailymotion.com/player/x1.js", "dailymotion"),
            ("https://www.youtube-nocookie.com/embed/abc", "youtube"),
            ("https://player.vimeo.com/video/1", "vimeo"),
            ("https://vjs.zencdn.net/7.0/video.min.js", "videojs"),
            ("https://cdn.example.gr/lib/hls.min.js", "hlsjs"),
        ];
        for (url, want) in table {
            let mut s = page();
            s.network.push(NetworkEntry { url: url.to_string(), ..Default::default() });
            assert_eq!(player_family(&s), Some(want), "{url}");
        }
        // all fifteen families are in the table
        assert_eq!(FAMILIES.len(), table.len());
        for (family, _) in FAMILIES {
            assert!(table.iter().any(|(_, f)| f == family), "{family} has no table row");
        }
    }

    #[test]
    fn a_page_with_two_players_gets_the_higher_priority_one() {
        let mut s = page();
        // hls.js is lower priority than jwplayer, whichever the page mentions first
        s.network.push(NetworkEntry { url: "https://cdn.example.gr/hls.min.js".into(), ..Default::default() });
        s.scripts.push("jwplayer.js".into());
        assert_eq!(player_family(&s), Some("jwplayer"));
        // a Brightcove page runs video.js too
        let mut b = page();
        b.html = Some("<div class=\"video-js vjs-default\"></div><script src=\"https://players.brightcove.net/1/x.js\">".into());
        assert_eq!(player_family(&b), Some("brightcove"));
        // without the Brightcove script the same markup is video.js
        let mut v = page();
        v.html = Some("<div class=\"video-js vjs-default\"></div>".into());
        assert_eq!(player_family(&v), Some("videojs"));
    }

    #[test]
    fn a_js_signature_must_end_the_file_name() {
        let net = |u: &str| {
            let mut s = page();
            s.network.push(NetworkEntry { url: u.to_string(), ..Default::default() });
            s
        };
        assert_eq!(player_family(&net("https://x.example.gr/api/video.json")), None);
        assert_eq!(player_family(&net("https://x/hls.json")), None);
        assert_eq!(player_family(&net("https://x.example.gr/video.min.js?v=3")), Some("videojs"));
        let mut s = page();
        s.html = Some("<script src=\"/lib/hls.min.js\"></script>".into());
        assert_eq!(player_family(&s), Some("hlsjs"));
        // a later, real match still counts after a false one
        let mut t = page();
        t.scripts.push("fetch('/api/video.json'); load('video.js')".into());
        assert_eq!(player_family(&t), Some("videojs"));
    }

    #[test]
    fn a_page_with_no_known_player_has_no_family() {
        assert_eq!(player_family(&page()), None);
        let mut s = page();
        s.html = Some("<html><body><video src=\"/a.mp4\"></video><p>players of the game</p></body></html>".into());
        s.network.push(NetworkEntry { url: "https://cdn.example.gr/a.mp4".into(), ..Default::default() });
        s.scripts.push("var x = 1;".into());
        assert_eq!(player_family(&s), None);
    }
}
