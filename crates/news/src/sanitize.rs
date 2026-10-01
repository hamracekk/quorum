//! Turning untrusted feed content into plain, bounded text and canonical http(s) links.

use reqwest::Url;

/// Query parameters that only track clicks; dropped so one article has one URL.
const TRACKING: [&str; 9] = [
    "fbclid", "gclid", "mc_cid", "mc_eid", "cmpid", "ref", "ref_src", "source", "spm",
];

/// Plain text from feed HTML: tags removed (with the contents of `script`/`style`), common
/// entities decoded, whitespace collapsed, and cut at a word boundary to `max` characters.
pub fn plain_text(html: &str, max: usize) -> String {
    let without_blocks = strip_blocks(html, "script");
    let without_blocks = strip_blocks(&without_blocks, "style");
    let mut text = String::with_capacity(without_blocks.len());
    let mut in_tag = false;
    for c in without_blocks.chars() {
        match c {
            '<' => {
                in_tag = true;
                text.push(' ');
            }
            '>' if in_tag => in_tag = false,
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    let decoded = decode_entities(&text);
    let collapsed = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&collapsed, max)
}

/// Remove `<tag …>…</tag>` blocks (case-insensitive), including unterminated ones.
fn strip_blocks(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let (open, close) = (format!("<{tag}"), format!("</{tag}>"));
    let mut out = String::with_capacity(html.len());
    let mut pos = 0;
    while let Some(start) = lower[pos..].find(&open).map(|i| i + pos) {
        out.push_str(&html[pos..start]);
        pos = lower[start..]
            .find(&close)
            .map_or(html.len(), |i| start + i + close.len());
    }
    out.push_str(&html[pos..]);
    out
}

fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let window: String = tail.chars().take(12).collect();
        let Some(semi) = window.find(';') else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let entity = &tail[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        if let Some(c) = decoded {
            out.push(c);
            rest = &tail[semi + 1..];
        } else {
            out.push('&');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

/// At most `max` characters, cut at the last word boundary, with an ellipsis when cut.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    let at_word = cut.rfind(' ').map_or(cut.as_str(), |i| &cut[..i]);
    format!("{}…", at_word.trim_end())
}

/// The canonical form of an article link: http(s) only, no fragment, no tracking parameters.
/// `None` for anything else (relative, other schemes, malformed).
pub fn canonical_url(raw: &str) -> Option<String> {
    let mut url = Url::parse(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    url.set_fragment(None);
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !k.starts_with("utm_") && !TRACKING.contains(&k.as_ref()))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if kept.is_empty() {
        url.set_query(None);
    } else {
        url.query_pairs_mut().clear().extend_pairs(kept);
    }
    Some(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_becomes_plain_text() {
        let html = "<p>Bitcoin <b>rallies</b>&nbsp;&amp; ETH &#8212; up&#x21;</p>\
                    <script>alert('x')</script><style>p{}</style><img src=x onerror=y>";
        assert_eq!(plain_text(html, 200), "Bitcoin rallies & ETH — up!");
    }

    #[test]
    fn unknown_entities_and_stray_ampersands_survive_as_text() {
        assert_eq!(plain_text("AT&T &bogus; 5 & 6", 100), "AT&T &bogus; 5 & 6");
    }

    #[test]
    fn long_text_is_cut_at_a_word() {
        let cut = plain_text("one two three four five", 12);
        assert_eq!(cut, "one two…");
        assert!(cut.chars().count() <= 12);
        assert_eq!(plain_text("short", 12), "short");
    }

    #[test]
    fn unterminated_script_blocks_are_dropped_entirely() {
        assert_eq!(plain_text("safe <script>steal()", 50), "safe");
    }

    #[test]
    fn urls_are_canonical_http_without_tracking() {
        assert_eq!(
            canonical_url("https://News.Example.com/a/b?utm_source=x&id=7&fbclid=y#top").as_deref(),
            Some("https://news.example.com/a/b?id=7")
        );
        assert_eq!(
            canonical_url("http://example.com/x?utm_medium=rss").as_deref(),
            Some("http://example.com/x")
        );
        for bad in [
            "javascript:alert(1)",
            "ftp://example.com/x",
            "/relative",
            "not a url",
            "",
        ] {
            assert_eq!(canonical_url(bad), None, "{bad}");
        }
    }
}
