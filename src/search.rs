//! Keyless web search (DuckDuckGo / Bing / Mojeek / Brave).
//!
//! Each engine is queried in parallel with its own timeout so one slow or
//! blocked engine never fails the whole search. Results are merged with
//! reciprocal-rank fusion, deduped by normalized URL, then reranked by how
//! many distinct query terms each hit covers.
//!
//! Every engine is classified (`ok:<n>` / `empty` / `blocked:<reason>` /
//! `timeout`) and the per-engine statuses are returned alongside the hits so
//! the caller can tell a block apart from a genuine no-results. DDG's main
//! html endpoint rate-limits aggressively (HTTP 202 anomaly page); when it
//! fails we retry once via the DDG lite endpoint.

use crate::fetcher;
use crate::normalize_url;
use crate::SearchHit;
use base64::Engine as _;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

/// Per-engine fetch timeout. One blocked engine must not stall the search.
const ENGINE_TIMEOUT: Duration = Duration::from_secs(6);
/// Reciprocal-rank-fusion constant (standard k = 60).
const RRF_K: f32 = 60.0;

// ── selectors ───────────────────────────────────────────────────────────────
macro_rules! sel {
    ($name:ident, $css:expr) => {
        fn $name() -> &'static scraper::Selector {
            static S: OnceLock<scraper::Selector> = OnceLock::new();
            S.get_or_init(|| scraper::Selector::parse($css).unwrap())
        }
    };
}
sel!(ddg_result_sel, ".result");
sel!(ddg_title_sel, "h2.result__title a");
sel!(ddg_snippet_sel, ".result__snippet");
sel!(a_href_sel, "a[href]");
sel!(bing_algo_sel, "li.b_algo");
sel!(bing_title_sel, "h2 a");
sel!(bing_caption_sel, ".b_caption p, .b_lineclamp2, .b_lineclamp3, .b_lineclamp4");
sel!(mojeek_result_sel, "ul.results-standard li");
sel!(mojeek_title_sel, "a.title");
sel!(mojeek_snippet_sel, "p.s");
// Brave renders results with Svelte (hashed classes); select on the stable
// semantic attributes/prefixes only.
sel!(brave_result_sel, "div[data-type=\"web\"]");
sel!(brave_title_sel, ".search-snippet-title");
sel!(brave_snippet_sel, ".snippet-description, .generic-snippet");
// DDG lite is a flat table: one `a.result-link` + one `td.result-snippet` per hit.
sel!(ddglite_link_sel, "a.result-link");
sel!(ddglite_snippet_sel, ".result-snippet");

fn text_of(el: scraper::ElementRef) -> String {
    el.text().collect::<Vec<_>>().join(" ").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Host-level junk: engine self-links and ad redirects that must never surface.
fn is_junk_url(url: &str) -> bool {
    if url.contains("duckduckgo.com/y.js") {
        return true;
    }
    let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(|s| s.to_lowercase())).unwrap_or_default();
    host.ends_with("bing.com") || host.ends_with("duckduckgo.com") || host.ends_with("mojeek.com") || host.ends_with("brave.com") || host.is_empty()
}

fn push_hit(hits: &mut Vec<SearchHit>, title: String, url: String, snippet: String) {
    if title.is_empty() || !url.starts_with("http") || is_junk_url(&url) {
        return;
    }
    hits.push(SearchHit { title, url, snippet });
}

// ── per-engine parsers ──────────────────────────────────────────────────────

pub(crate) fn parse_ddg(html: &str) -> Vec<SearchHit> {
    let doc = scraper::Html::parse_document(html);
    let mut hits = Vec::new();
    for result in doc.select(ddg_result_sel()) {
        let Some(title_el) = result.select(ddg_title_sel()).next() else { continue };
        let title = text_of(title_el);
        let href = title_el.value().attr("href").unwrap_or("");
        let url = resolve_ddg_href(href);
        let snippet = result.select(ddg_snippet_sel()).next().map(text_of).unwrap_or_default();
        push_hit(&mut hits, title, url, snippet);
    }
    if hits.is_empty() {
        for el in doc.select(a_href_sel()) {
            let href = resolve_ddg_href(el.value().attr("href").unwrap_or("").trim());
            let title = text_of(el);
            if title.len() < 10 || title.len() > 200 {
                continue;
            }
            push_hit(&mut hits, title, href, String::new());
        }
    }
    hits
}

fn resolve_ddg_href(href: &str) -> String {
    let mut href = href.trim().to_string();
    if href.starts_with("//") {
        href = format!("https:{href}");
    }
    if href.contains("uddg=")
        && let Some(enc) = href.split("uddg=").nth(1).and_then(|s| s.split('&').next())
    {
        return urlencoding_decode(enc);
    }
    href
}

pub(crate) fn parse_bing(html: &str) -> Vec<SearchHit> {
    let doc = scraper::Html::parse_document(html);
    let mut hits = Vec::new();
    for algo in doc.select(bing_algo_sel()) {
        let Some(a) = algo.select(bing_title_sel()).next() else { continue };
        let title = text_of(a);
        let url = decode_bing_href(a.value().attr("href").unwrap_or(""));
        let snippet = algo.select(bing_caption_sel()).next().map(text_of).unwrap_or_default();
        push_hit(&mut hits, title, url, snippet);
    }
    hits
}

/// Bing wraps targets in `bing.com/ck/a?...&u=a1<base64url>`. Strip the `a1`
/// prefix and base64url-decode (no padding) to recover the real URL.
pub(crate) fn decode_bing_href(href: &str) -> String {
    let href = href.trim();
    if !href.contains("bing.com/ck/a") {
        return href.to_string();
    }
    let Some(u) = href.split("u=").nth(1).and_then(|s| s.split('&').next()) else {
        return href.to_string();
    };
    let b64 = u.strip_prefix("a1").unwrap_or(u);
    match base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(b64) {
        Ok(bytes) => {
            let s = String::from_utf8_lossy(&bytes).into_owned();
            if s.starts_with("http") { s } else { href.to_string() }
        }
        Err(_) => href.to_string(),
    }
}

pub(crate) fn parse_mojeek(html: &str) -> Vec<SearchHit> {
    let doc = scraper::Html::parse_document(html);
    let mut hits = Vec::new();
    for li in doc.select(mojeek_result_sel()) {
        let Some(a) = li.select(mojeek_title_sel()).next() else { continue };
        let title = text_of(a);
        let url = a.value().attr("href").unwrap_or("").trim().to_string();
        let snippet = li.select(mojeek_snippet_sel()).next().map(text_of).unwrap_or_default();
        push_hit(&mut hits, title, url, snippet);
    }
    hits
}

/// Each web result is a `div[data-type="web"]`; the first anchor in it holds the
/// (already-direct) target URL, `.search-snippet-title` the title, and
/// `.generic-snippet` / `.snippet-description` the description.
pub(crate) fn parse_brave(html: &str) -> Vec<SearchHit> {
    let doc = scraper::Html::parse_document(html);
    let mut hits = Vec::new();
    for res in doc.select(brave_result_sel()) {
        let Some(a) = res.select(a_href_sel()).next() else { continue };
        let url = a.value().attr("href").unwrap_or("").trim().to_string();
        let title = res.select(brave_title_sel()).next().map(text_of).unwrap_or_else(|| text_of(a));
        let snippet = res.select(brave_snippet_sel()).next().map(text_of).unwrap_or_default();
        push_hit(&mut hits, title, url, snippet);
    }
    hits
}

/// DDG lite has no result containers — links and snippets are separate table
/// cells in document order, so zip them positionally. URLs are the same
/// `/l/?uddg=` redirects as the main endpoint.
pub(crate) fn parse_ddg_lite(html: &str) -> Vec<SearchHit> {
    let doc = scraper::Html::parse_document(html);
    let snippets: Vec<String> = doc.select(ddglite_snippet_sel()).map(text_of).collect();
    let mut hits = Vec::new();
    for (i, a) in doc.select(ddglite_link_sel()).enumerate() {
        let title = text_of(a);
        let url = resolve_ddg_href(a.value().attr("href").unwrap_or(""));
        let snippet = snippets.get(i).cloned().unwrap_or_default();
        push_hit(&mut hits, title, url, snippet);
    }
    hits
}

/// Known captcha / anomaly / bot-wall markers in an otherwise-200 body. Returns
/// a short reason tag when the body is a block page rather than real results.
pub(crate) fn detect_block(html: &str) -> Option<&'static str> {
    let h = html.to_lowercase();
    if h.contains("anomaly.js") || h.contains("/anomaly") {
        return Some("anomaly");
    }
    if h.contains("/cdn-cgi/challenge") || h.contains("challenge-platform") || h.contains("cf-chl") {
        return Some("cloudflare");
    }
    if h.contains("g-recaptcha") || h.contains("recaptcha/api") || h.contains("hcaptcha") {
        return Some("captcha");
    }
    if h.contains("unusual traffic") || h.contains("are you a robot") || h.contains("verify you are a human") {
        return Some("captcha");
    }
    None
}

/// Classify one engine fetch into (hits, status) where status is one of
/// `ok:<n>` / `empty` / `blocked:<reason>` / `timeout`.
fn classify(resp: Option<(u16, String)>, parse: impl Fn(&str) -> Vec<SearchHit>) -> (Vec<SearchHit>, String) {
    match resp {
        None => (Vec::new(), "timeout".into()),
        Some((status, body)) => {
            if status != 200 {
                return (Vec::new(), format!("blocked:http{status}"));
            }
            if let Some(reason) = detect_block(&body) {
                return (Vec::new(), format!("blocked:{reason}"));
            }
            let hits = parse(&body);
            if hits.is_empty() {
                (Vec::new(), "empty".into())
            } else {
                let n = hits.len();
                (hits, format!("ok:{n}"))
            }
        }
    }
}

// ── ranking ─────────────────────────────────────────────────────────────────

const STOPWORDS: &[&str] = &[
    "vs", "versus", "and", "or", "the", "a", "an", "of", "to", "for", "in", "on",
    "with", "is", "are", "how", "what", "why", "best", "using", "use",
];

/// Distinct, lowercased query terms, minus stopwords and bare years/numbers.
fn query_terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for raw in query.split_whitespace() {
        let t = raw.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
        if t.len() < 2 || STOPWORDS.contains(&t.as_str()) {
            continue;
        }
        if t.chars().all(|c| c.is_ascii_digit()) {
            continue; // years / bare numbers
        }
        if !terms.contains(&t) {
            terms.push(t);
        }
    }
    terms
}

const DEMOTE_HOSTS: &[&str] = &["youtube.com", "youtu.be", "pinterest.", "facebook.com", "tiktok.com", "quora.com"];

fn is_demoted(url: &str) -> bool {
    let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(|s| s.to_lowercase())).unwrap_or_default();
    DEMOTE_HOSTS.iter().any(|d| host.contains(d))
}

/// Fraction of distinct query terms appearing in title+snippet+url.
fn coverage(hit: &SearchHit, terms: &[String]) -> f32 {
    if terms.is_empty() {
        return 0.0;
    }
    let hay = format!("{} {} {}", hit.title, hit.snippet, hit.url).to_lowercase();
    let found = terms.iter().filter(|t| hay.contains(t.as_str())).count();
    found as f32 / terms.len() as f32
}

struct Agg {
    hit: SearchHit,
    rrf: f32,
}

/// Merge engine result lists with reciprocal-rank fusion, dedupe by normalized
/// URL, merge snippets, then rerank: coverage dominates, RRF breaks ties,
/// social hosts are mildly demoted.
fn fuse_and_rank(engine_hits: Vec<Vec<SearchHit>>, terms: &[String]) -> Vec<SearchHit> {
    let mut map: HashMap<String, Agg> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for hits in engine_hits {
        for (rank, hit) in hits.into_iter().enumerate() {
            let key = normalize_url(&hit.url);
            let contrib = 1.0 / (RRF_K + rank as f32);
            match map.get_mut(&key) {
                Some(agg) => {
                    agg.rrf += contrib;
                    if agg.hit.snippet.is_empty() && !hit.snippet.is_empty() {
                        agg.hit.snippet = hit.snippet;
                    }
                    if hit.title.len() > agg.hit.title.len() {
                        agg.hit.title = hit.title;
                    }
                }
                None => {
                    order.push(key.clone());
                    map.insert(key, Agg { hit, rrf: contrib });
                }
            }
        }
    }
    let mut aggs: Vec<Agg> = order.into_iter().filter_map(|k| map.remove(&k)).collect();
    aggs.sort_by(|a, b| score(a, terms).partial_cmp(&score(b, terms)).unwrap_or(std::cmp::Ordering::Equal).reverse());
    aggs.into_iter().map(|a| a.hit).collect()
}

/// Coverage is weighted ×10 so it strictly outranks RRF (max ~0.05/engine):
/// a hit covering both query terms always beats one covering a single term.
fn score(a: &Agg, terms: &[String]) -> f32 {
    let mut s = coverage(&a.hit, terms) * 10.0 + a.rrf;
    if is_demoted(&a.hit.url) {
        s -= 0.5;
    }
    s
}

// ── entry point ─────────────────────────────────────────────────────────────

pub(crate) async fn search_run(
    query: &str,
    max_results: usize,
    site: Option<&str>,
    exclude_sites: Option<&[String]>,
    freshness: Option<&str>,
) -> anyhow::Result<(Vec<SearchHit>, Vec<String>)> {
    let max_results = max_results.clamp(1, 20);
    let client = fetcher::client();

    let mut effective = query.to_string();
    if let Some(s) = site {
        let s = s.trim();
        if !s.is_empty() {
            effective.push_str(&format!(" site:{s}"));
        }
    }
    if let Some(excludes) = exclude_sites {
        for ex in excludes {
            let ex = ex.trim();
            if !ex.is_empty() {
                effective.push_str(&format!(" -site:{ex}"));
            }
        }
    }
    if let Some(f) = freshness {
        let f = f.trim().to_lowercase();
        if matches!(f.as_str(), "day" | "week" | "month" | "year") {
            effective.push_str(&format!(" when:{f}"));
        }
    }
    let q = urlencoding(&effective);
    let ddg_url = format!("https://html.duckduckgo.com/html/?q={q}");
    let bing_url = format!("https://www.bing.com/search?q={q}");
    let mojeek_url = format!("https://www.mojeek.com/search?q={q}");
    let brave_url = format!("https://search.brave.com/search?q={q}&source=web");

    // Fetch returns (status, body) so the caller can distinguish a 202/403 wall
    // from a 200 captcha page from a timeout.
    let fetch = |url: String| async move {
        let fut = async {
            let resp = client.get(&url).send().await.ok()?;
            let status = resp.status().as_u16();
            let body = resp.text().await.ok()?;
            Some((status, body))
        };
        tokio::time::timeout(ENGINE_TIMEOUT, fut).await.ok().flatten()
    };
    let (ddg, bing, mojeek, brave) =
        tokio::join!(fetch(ddg_url), fetch(bing_url), fetch(mojeek_url), fetch(brave_url));

    // DDG: if the main html endpoint is blocked/empty/timed out, retry once via
    // the lite endpoint before giving up on DuckDuckGo entirely.
    let (ddg_hits, ddg_status) = {
        let (hits, status) = classify(ddg, parse_ddg);
        if status.starts_with("ok") {
            (hits, format!("ddg:{status}"))
        } else {
            let lite_url = format!("https://lite.duckduckgo.com/lite/?q={q}");
            let (lhits, lstatus) = classify(fetch(lite_url).await, parse_ddg_lite);
            if lstatus.starts_with("ok") {
                (lhits, format!("ddg-lite:{lstatus}"))
            } else {
                (hits, format!("ddg:{status}"))
            }
        }
    };
    let (bing_hits, bing_status) = classify(bing, parse_bing);
    let (brave_hits, brave_status) = classify(brave, parse_brave);
    let (mojeek_hits, mojeek_status) = classify(mojeek, parse_mojeek);

    let engines = vec![
        ddg_status.clone(),
        format!("bing:{bing_status}"),
        format!("brave:{brave_status}"),
        format!("mojeek:{mojeek_status}"),
    ];
    // Every engine failed (none returned ok) — surface the block, don't lie
    // "no results".
    let any_ok = ddg_status.contains(":ok:") || bing_status.starts_with("ok") || brave_status.starts_with("ok") || mojeek_status.starts_with("ok");
    if !any_ok {
        anyhow::bail!("all search engines failed: [{}]", engines.join(", "));
    }

    let terms = query_terms(query);
    let mut merged = fuse_and_rank(vec![ddg_hits, bing_hits, brave_hits, mojeek_hits], &terms);

    if let Some(s) = site {
        let s = s.trim().to_lowercase();
        if !s.is_empty() {
            merged.retain(|h| {
                let host = url::Url::parse(&h.url).ok().and_then(|u| u.host_str().map(|s| s.to_lowercase())).unwrap_or_default();
                host.contains(&s) || h.url.to_lowercase().contains(&s)
            });
        }
    }
    if let Some(excludes) = exclude_sites {
        let lowers: Vec<String> = excludes.iter().map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty()).collect();
        if !lowers.is_empty() {
            merged.retain(|h| {
                let url_lower = h.url.to_lowercase();
                let host = url::Url::parse(&h.url).ok().and_then(|u| u.host_str().map(|s| s.to_lowercase())).unwrap_or_default();
                !lowers.iter().any(|ex| host.contains(ex) || url_lower.contains(ex))
            });
        }
    }

    merged.truncate(max_results);
    Ok((merged, engines))
}

// ── url encoding ────────────────────────────────────────────────────────────

fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' {
            out.push(b as char);
        } else if b == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Percent-decode into bytes, then interpret as UTF-8 (lossy). Decoding byte by
/// byte into `char`s would mangle any multi-byte UTF-8 sequence.
pub(crate) fn urlencoding_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    out.push(b);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_decode_multibyte() {
        // %C3%A9 = é, %E2%80%94 = em dash
        assert_eq!(urlencoding_decode("caf%C3%A9"), "café");
        assert_eq!(urlencoding_decode("a%E2%80%94b"), "a—b");
        assert_eq!(urlencoding_decode("a+b%2Fc"), "a b/c");
    }

    #[test]
    fn bing_ck_decode() {
        // base64url(no pad) of https://github.com/tokio-rs/axum
        let href = "https://www.bing.com/ck/a?!&&p=xx&u=a1aHR0cHM6Ly9naXRodWIuY29tL3Rva2lvLXJzL2F4dW0";
        assert_eq!(decode_bing_href(href), "https://github.com/tokio-rs/axum");
        // non-ck passes through
        assert_eq!(decode_bing_href("https://example.com/x"), "https://example.com/x");
    }

    #[test]
    fn ddg_fixture_parses() {
        let html = include_str!("../tests/fixtures/ddg.html");
        let hits = parse_ddg(html);
        assert!(hits.len() >= 2, "got {}", hits.len());
        assert!(hits.iter().all(|h| h.url.starts_with("http")));
        assert!(hits.iter().all(|h| !is_junk_url(&h.url)));
        assert!(hits.iter().any(|h| !h.snippet.is_empty()), "expected snippets");
    }

    #[test]
    fn bing_fixture_parses_and_decodes() {
        let html = include_str!("../tests/fixtures/bing.html");
        let hits = parse_bing(html);
        assert!(hits.len() >= 2, "got {}", hits.len());
        assert!(hits.iter().all(|h| !h.url.contains("bing.com")), "ck/a not decoded");
        assert!(hits.iter().any(|h| !h.snippet.is_empty()), "expected snippets");
    }

    #[test]
    fn brave_fixture_parses() {
        let html = include_str!("../tests/fixtures/brave.html");
        let hits = parse_brave(html);
        assert!(hits.len() >= 4, "got {}", hits.len());
        assert!(hits.iter().all(|h| h.url.starts_with("http")));
        assert!(hits.iter().all(|h| !h.url.contains("brave.com")), "internal brave link leaked");
        assert_eq!(hits[0].url, "https://docs.rs/tokio/latest/tokio/macro.select.html");
        assert!(hits.iter().any(|h| !h.snippet.is_empty()), "expected snippets");
    }

    #[test]
    fn ddg_lite_fixture_parses() {
        let html = include_str!("../tests/fixtures/ddglite.html");
        let hits = parse_ddg_lite(html);
        assert_eq!(hits.len(), 3, "got {}", hits.len());
        // uddg redirect must be decoded to the real target.
        assert_eq!(hits[0].url, "https://docs.rs/tokio/latest/tokio/macro.select.html");
        assert!(hits.iter().all(|h| !h.url.contains("duckduckgo.com")), "redirect not resolved");
        assert!(hits.iter().all(|h| !h.snippet.is_empty()), "expected snippets");
    }

    #[test]
    fn block_detection() {
        let blocked = include_str!("../tests/fixtures/ddglite_blocked.html");
        assert_eq!(detect_block(blocked), Some("anomaly"));
        // Real result pages are not flagged.
        assert_eq!(detect_block(include_str!("../tests/fixtures/brave.html")), None);
        assert_eq!(detect_block(include_str!("../tests/fixtures/ddglite.html")), None);
        // Status-based classification.
        assert_eq!(classify(None, parse_brave).1, "timeout");
        assert_eq!(classify(Some((202, String::new())), parse_brave).1, "blocked:http202");
        assert_eq!(classify(Some((200, blocked.to_string())), parse_ddg_lite).1, "blocked:anomaly");
        assert_eq!(classify(Some((200, "<html></html>".into())), parse_brave).1, "empty");
        let (hits, st) = classify(Some((200, include_str!("../tests/fixtures/ddglite.html").to_string())), parse_ddg_lite);
        assert_eq!(st, "ok:3");
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn mojeek_fixture_parses() {
        let html = include_str!("../tests/fixtures/mojeek.html");
        let hits = parse_mojeek(html);
        assert!(hits.len() >= 2, "got {}", hits.len());
        assert_eq!(hits[0].url, "https://github.com/tokio-rs/axum");
        assert!(!hits[0].snippet.is_empty());
    }

    #[test]
    fn rrf_and_coverage_ordering() {
        let terms = query_terms("axum vs actix web 2025");
        assert_eq!(terms, vec!["axum", "actix", "web"]);
        // Covers both axum & actix (and web) — should win despite lower RRF.
        let compare = SearchHit {
            title: "Axum vs Actix Web comparison".into(),
            url: "https://blog.example.com/axum-actix-web".into(),
            snippet: "benchmark".into(),
        };
        // Covers only axum but is returned by all engines (high RRF).
        let wiki = SearchHit {
            title: "Axum — Wikipedia".into(),
            url: "https://en.wikipedia.org/wiki/Axum".into(),
            snippet: "a town".into(),
        };
        let out = fuse_and_rank(
            vec![vec![wiki.clone()], vec![wiki.clone()], vec![wiki.clone(), compare.clone()]],
            &terms,
        );
        assert_eq!(out[0].url, compare.url, "full-coverage hit must rank first");
    }

    #[test]
    fn social_hosts_demoted() {
        let terms = query_terms("rust tokio select");
        let article = SearchHit { title: "rust tokio select guide".into(), url: "https://docs.rs/tokio".into(), snippet: "select macro".into() };
        let yt = SearchHit { title: "rust tokio select video".into(), url: "https://www.youtube.com/watch?v=x".into(), snippet: "select macro".into() };
        // Same coverage & RRF; youtube demoted ⇒ article first.
        let out = fuse_and_rank(vec![vec![yt.clone()], vec![article.clone()]], &terms);
        assert_eq!(out[0].url, article.url);
    }
}
