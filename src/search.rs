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
use crate::scrape::normalize_url;
use crate::types::SearchHit;
use base64::Engine as _;
use fs2::FileExt;
use std::collections::HashMap;
use std::io::{Read, Seek};
use std::sync::OnceLock;
use std::time::Duration;

/// Per-engine fetch timeout. One blocked engine must not stall the search.
const ENGINE_TIMEOUT: Duration = Duration::from_secs(6);
/// Reciprocal-rank-fusion constant (standard k = 60).
const RRF_K: f32 = 60.0;

// ── cross-process engine state (pacing + cooldowns) ─────────────────────────
// Several `scour search` processes can run within seconds of each other (an LLM
// agent often fires two in one bash line). We persist per-engine timing in
// ~/.cache/scour/engines.json under an fs2 file lock so every process paces the
// same engines and honors cooldowns after a block/captcha, no matter who hit it.

/// Minimum spacing between two requests to the *same* engine, across processes.
// Pacing serializes *staggered* processes (each reads the shared last_ms before
// its own request), but gives no guarantee for *perfectly simultaneous* ones:
// two processes that read the state at the same instant both see the same
// last_ms and may fire together. This is best-effort smoothing, not a hard rate
// limit.
const PACE_MS: u64 = 1100;
/// Never sleep longer than this to satisfy pacing (keeps latency bounded).
const PACE_MAX_SLEEP_MS: u64 = 1500;
/// Cooldown after DDG blocks (it rate-limits hard and recovers slowly).
const COOLDOWN_DDG_MS: u64 = 10 * 60 * 1000;
/// Cooldown after any other engine blocks/captchas.
const COOLDOWN_OTHER_MS: u64 = 2 * 60 * 1000;

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
struct EngineState {
    #[serde(default)]
    last_ms: u64,
    #[serde(default)]
    cooldown_until_ms: u64,
}

type EngineStates = HashMap<String, EngineState>;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn engines_state_path() -> std::path::PathBuf {
    let base = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(".cache").join("scour").join("engines.json")
}

/// Load the engine-state map, tolerating a missing or corrupt file (returns an
/// empty map). Best-effort, no lock held after return.
fn load_states() -> EngineStates {
    let path = engines_state_path();
    let Ok(file) = std::fs::OpenOptions::new().read(true).open(&path) else {
        return EngineStates::new();
    };
    let _ = file.lock_shared();
    let mut s = String::new();
    let mut f = &file;
    let _ = f.seek(std::io::SeekFrom::Start(0));
    let _ = f.read_to_string(&mut s);
    let _ = FileExt::unlock(&file);
    serde_json::from_str(&s).unwrap_or_default()
}

/// Pure cooldown test so it can be unit-tested without touching the fs.
fn in_cooldown(st: &EngineState, now: u64) -> bool {
    st.cooldown_until_ms > now
}

/// Pure pacing math: how long to sleep so this engine's next request is >=
/// PACE_MS after `last_ms`, clamped to PACE_MAX_SLEEP_MS. Returns 0 when enough
/// time has already passed (or the clock went backwards).
fn pace_sleep_ms(last_ms: u64, now: u64) -> u64 {
    if last_ms == 0 || now < last_ms {
        return 0;
    }
    let elapsed = now - last_ms;
    if elapsed >= PACE_MS {
        0
    } else {
        (PACE_MS - elapsed).min(PACE_MAX_SLEEP_MS)
    }
}

/// Cooldown duration for an engine once it reports a block/captcha.
fn cooldown_ms_for(engine: &str) -> u64 {
    if engine.starts_with("ddg") {
        COOLDOWN_DDG_MS
    } else {
        COOLDOWN_OTHER_MS
    }
}

/// Read-modify-write the state file under an exclusive lock. Best-effort: on any
/// io/lock error we silently skip the update (pacing degrades, never breaks).
fn update_state(engine: &str, f: impl FnOnce(&mut EngineState)) {
    let path = engines_state_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
    else {
        return;
    };
    if file.lock_exclusive().is_err() {
        return;
    }
    let mut s = String::new();
    let _ = file.seek(std::io::SeekFrom::Start(0));
    let _ = file.read_to_string(&mut s);
    let mut states: EngineStates = serde_json::from_str(&s).unwrap_or_default();
    f(states.entry(engine.to_string()).or_default());
    if let Ok(json) = serde_json::to_string(&states) {
        // Atomic replace: write a temp file in the same dir, then rename over the
        // target (rename is atomic on the same filesystem) so readers never see a
        // half-written file. Still done while holding the fs2 lock on the original.
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json.as_bytes()).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
    let _ = FileExt::unlock(&file);
}

/// Mark that we are about to send a request to `engine` (updates last_ms).
fn record_request(engine: &str) {
    let now = now_ms();
    update_state(engine, |st| st.last_ms = now);
}

/// Put `engine` into cooldown after a block/captcha/429/202.
fn set_cooldown(engine: &str) {
    let until = now_ms() + cooldown_ms_for(engine);
    update_state(engine, |st| st.cooldown_until_ms = until);
}

/// Engine slots used for pacing/cooldown (must match the names passed to
/// `run_engine` / the ddg task in `search_run`).
const ENGINE_NAMES: &[&str] = &["ddg", "bing", "brave", "mojeek", "yahoo"];

/// When *every* engine is in cooldown, pick the engine(s) whose cooldown expires
/// soonest to force-run anyway (prefer the 2 soonest), so a search never returns
/// "all failed" without a live network attempt. Returns the engine names to
/// bypass; empty when at least one engine is already available. Pure so it can
/// be unit-tested without touching the fs.
fn cooldown_bypass(states: &EngineStates, now: u64) -> Vec<String> {
    let all_cooled = ENGINE_NAMES
        .iter()
        .all(|n| states.get(*n).map(|s| in_cooldown(s, now)).unwrap_or(false));
    if !all_cooled {
        return Vec::new();
    }
    let mut by_soonest: Vec<&str> = ENGINE_NAMES.to_vec();
    by_soonest.sort_by_key(|n| states.get(*n).map(|s| s.cooldown_until_ms).unwrap_or(0));
    by_soonest
        .into_iter()
        .take(2)
        .map(|s| s.to_string())
        .collect()
}

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
sel!(
    bing_caption_sel,
    ".b_caption p, .b_lineclamp2, .b_lineclamp3, .b_lineclamp4"
);
sel!(mojeek_result_sel, "ul.results-standard li");
sel!(mojeek_title_sel, "a.title");
sel!(mojeek_snippet_sel, "p.s");
// Brave renders results with Svelte (hashed classes); select on the stable
// semantic attributes/prefixes only.
sel!(brave_result_sel, "div[data-type=\"web\"]");
sel!(brave_title_sel, ".search-snippet-title");
sel!(brave_snippet_sel, ".snippet-description, .generic-snippet");
// DDG lite is a table: each hit is a `tr` with an `a.result-link`, followed by
// a `tr` with a `td.result-snippet`. Walk rows so a missing snippet row doesn't
// shift later snippets onto the wrong link.
sel!(ddglite_row_sel, "tr");
sel!(ddglite_link_sel, "a.result-link");
sel!(ddglite_snippet_sel, ".result-snippet");
// Yahoo: each web result is a `div.algo`; the title anchor lives in
// `div.compTitle a` and wraps an `h3.title`, the snippet in `div.compText`.
// hrefs are `r.search.yahoo.com/.../RU=<urlencoded target>/RK=...` redirects.
sel!(yahoo_result_sel, "div.algo");
sel!(yahoo_link_sel, "div.compTitle a[href]");
sel!(yahoo_title_sel, "h3.title");
sel!(yahoo_snippet_sel, "div.compText");

fn text_of(el: scraper::ElementRef) -> String {
    el.text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Host-level junk: engine self-links and ad redirects that must never surface.
fn is_junk_url(url: &str) -> bool {
    if url.contains("duckduckgo.com/y.js") {
        return true;
    }
    let host = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_lowercase()))
        .unwrap_or_default();
    host.ends_with("bing.com")
        || host.ends_with("duckduckgo.com")
        || host.ends_with("mojeek.com")
        || host.ends_with("brave.com")
        || host.ends_with("search.yahoo.com")
        || host.is_empty()
}

/// Lowercased host of a URL, or empty when it won't parse.
fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_lowercase()))
        .unwrap_or_default()
}

/// True when `host` is `domain` or a subdomain of it. `domain` may carry a
/// leading `www.`/dots (stripped first) so `example.com` matches `example.com`
/// and `a.example.com` but not `notexample.com`.
fn host_matches(host: &str, domain: &str) -> bool {
    let d = domain.trim().to_lowercase();
    let d = d
        .trim_matches('.')
        .trim_start_matches("www.")
        .trim_matches('.');
    !d.is_empty() && (host == d || host.ends_with(&format!(".{d}")))
}

fn push_hit(hits: &mut Vec<SearchHit>, title: String, url: String, snippet: String) {
    if title.is_empty() || !url.starts_with("http") || is_junk_url(&url) {
        return;
    }
    hits.push(SearchHit {
        title,
        url,
        snippet,
    });
}

// ── per-engine parsers ──────────────────────────────────────────────────────

pub(crate) fn parse_ddg(html: &str) -> Vec<SearchHit> {
    let doc = scraper::Html::parse_document(html);
    let mut hits = Vec::new();
    for result in doc.select(ddg_result_sel()) {
        let Some(title_el) = result.select(ddg_title_sel()).next() else {
            continue;
        };
        let title = text_of(title_el);
        let href = title_el.value().attr("href").unwrap_or("");
        let url = resolve_ddg_href(href);
        let snippet = result
            .select(ddg_snippet_sel())
            .next()
            .map(text_of)
            .unwrap_or_default();
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
    let href = href.trim();
    // Join against the DDG base so root-relative (`/l/?uddg=...`),
    // protocol-relative (`//x`), and absolute hrefs all parse.
    let base = url::Url::parse("https://duckduckgo.com/").expect("static base");
    if let Ok(u) = base.join(href)
        && let Some((_, v)) = u.query_pairs().find(|(k, _)| k == "uddg")
    {
        return v.into_owned();
    }
    href.to_string()
}

pub(crate) fn parse_bing(html: &str) -> Vec<SearchHit> {
    let doc = scraper::Html::parse_document(html);
    let mut hits = Vec::new();
    for algo in doc.select(bing_algo_sel()) {
        let Some(a) = algo.select(bing_title_sel()).next() else {
            continue;
        };
        let title = text_of(a);
        let url = decode_bing_href(a.value().attr("href").unwrap_or(""));
        let snippet = algo
            .select(bing_caption_sel())
            .next()
            .map(text_of)
            .unwrap_or_default();
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
    let Ok(url) = url::Url::parse(href) else {
        return href.to_string();
    };
    let Some((_, u)) = url.query_pairs().find(|(k, _)| k == "u") else {
        return href.to_string();
    };
    let b64 = u.strip_prefix("a1").unwrap_or(&u);
    match base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(b64) {
        Ok(bytes) => {
            let s = String::from_utf8_lossy(&bytes).into_owned();
            if s.starts_with("http") {
                s
            } else {
                href.to_string()
            }
        }
        Err(_) => href.to_string(),
    }
}

pub(crate) fn parse_yahoo(html: &str) -> Vec<SearchHit> {
    let doc = scraper::Html::parse_document(html);
    let mut hits = Vec::new();
    for res in doc.select(yahoo_result_sel()) {
        let Some(a) = res.select(yahoo_link_sel()).next() else {
            continue;
        };
        let url = decode_yahoo_href(a.value().attr("href").unwrap_or(""));
        let title = res
            .select(yahoo_title_sel())
            .next()
            .map(text_of)
            .unwrap_or_else(|| text_of(a));
        let snippet = res
            .select(yahoo_snippet_sel())
            .next()
            .map(text_of)
            .unwrap_or_default();
        push_hit(&mut hits, title, url, snippet);
    }
    hits
}

/// Yahoo wraps targets in `r.search.yahoo.com/...;_ylu=.../RU=<pct-encoded>/RK=...`.
/// Extract the `RU=` segment (up to the next `/`) and percent-decode it.
pub(crate) fn decode_yahoo_href(href: &str) -> String {
    let href = href.trim();
    let Some(pos) = href.find("RU=") else {
        return href.to_string();
    };
    let rest = &href[pos + 3..];
    let end = rest.find('/').unwrap_or(rest.len());
    let decoded = percent_decode(&rest[..end]);
    if decoded.starts_with("http") {
        decoded
    } else {
        href.to_string()
    }
}

/// Minimal `%XX` percent-decoder (Yahoo uses lowercase hex). Non-escape bytes
/// pass through unchanged; invalid UTF-8 is lossily replaced.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // Operate on bytes, never str slices: a multibyte char right after '%'
        // would make `&s[i+1..i+3]` fall on a non-char-boundary and panic. Only
        // decode when both following bytes are ASCII hex; otherwise keep '%'.
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
        {
            let hi = (bytes[i + 1] as char).to_digit(16).unwrap() as u8;
            let lo = (bytes[i + 2] as char).to_digit(16).unwrap() as u8;
            out.push(hi << 4 | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn parse_mojeek(html: &str) -> Vec<SearchHit> {
    let doc = scraper::Html::parse_document(html);
    let mut hits = Vec::new();
    for li in doc.select(mojeek_result_sel()) {
        let Some(a) = li.select(mojeek_title_sel()).next() else {
            continue;
        };
        let title = text_of(a);
        let url = a.value().attr("href").unwrap_or("").trim().to_string();
        let snippet = li
            .select(mojeek_snippet_sel())
            .next()
            .map(text_of)
            .unwrap_or_default();
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
        let Some(a) = res.select(a_href_sel()).next() else {
            continue;
        };
        let url = a.value().attr("href").unwrap_or("").trim().to_string();
        let title = res
            .select(brave_title_sel())
            .next()
            .map(text_of)
            .unwrap_or_else(|| text_of(a));
        let snippet = res
            .select(brave_snippet_sel())
            .next()
            .map(text_of)
            .unwrap_or_default();
        push_hit(&mut hits, title, url, snippet);
    }
    hits
}

/// DDG lite is a table where each hit is a link row optionally followed by a
/// snippet row. Walk rows and attach each snippet to the link it follows, so a
/// result lacking a snippet doesn't shift later snippets onto the wrong link.
/// URLs are the same `/l/?uddg=` redirects as the main endpoint.
pub(crate) fn parse_ddg_lite(html: &str) -> Vec<SearchHit> {
    let doc = scraper::Html::parse_document(html);
    let mut hits = Vec::new();
    let mut pending: Option<(String, String)> = None; // (title, url) awaiting snippet
    for row in doc.select(ddglite_row_sel()) {
        if let Some(a) = row.select(ddglite_link_sel()).next() {
            if let Some((title, url)) = pending.take() {
                push_hit(&mut hits, title, url, String::new());
            }
            let title = text_of(a);
            let url = resolve_ddg_href(a.value().attr("href").unwrap_or(""));
            pending = Some((title, url));
        } else if let Some(snip) = row.select(ddglite_snippet_sel()).next()
            && let Some((title, url)) = pending.take()
        {
            push_hit(&mut hits, title, url, text_of(snip));
        }
    }
    if let Some((title, url)) = pending.take() {
        push_hit(&mut hits, title, url, String::new());
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
    if h.contains("/cdn-cgi/challenge") || h.contains("challenge-platform") || h.contains("cf-chl")
    {
        return Some("cloudflare");
    }
    if h.contains("g-recaptcha") || h.contains("recaptcha/api") || h.contains("hcaptcha") {
        return Some("captcha");
    }
    if h.contains("unusual traffic")
        || h.contains("are you a robot")
        || h.contains("verify you are a human")
    {
        return Some("captcha");
    }
    None
}

/// Classify one engine fetch into (hits, status) where status is one of
/// `ok:<n>` / `empty` / `blocked:<reason>` / `error:http<status>` / `timeout`.
/// Only real blocks (202 DDG anomaly, 403, 429, captcha/anomaly markers) are
/// reported as `blocked:*` (the caller opens a cooldown on those). Transient 5xx
/// and other non-200s become `error:http<status>` with no cooldown, so one flaky
/// 502/503 never blacks out an engine for minutes.
fn classify(
    resp: Option<(u16, String)>,
    parse: impl Fn(&str) -> Vec<SearchHit>,
) -> (Vec<SearchHit>, String) {
    match resp {
        None => (Vec::new(), "timeout".into()),
        Some((status, body)) => {
            if status != 200 {
                return if matches!(status, 202 | 403 | 429) {
                    (Vec::new(), format!("blocked:http{status}"))
                } else {
                    (Vec::new(), format!("error:http{status}"))
                };
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
    "vs", "versus", "and", "or", "the", "a", "an", "of", "to", "for", "in", "on", "with", "is",
    "are", "how", "what", "why", "best", "using", "use",
];

/// Distinct, lowercased query terms, minus stopwords and bare years/numbers.
fn query_terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for raw in query.split_whitespace() {
        let t = raw
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase();
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

const DEMOTE_HOSTS: &[&str] = &[
    "youtube.com",
    "youtu.be",
    "pinterest.",
    "facebook.com",
    "tiktok.com",
    "quora.com",
];

fn is_demoted(url: &str) -> bool {
    let host = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_lowercase()))
        .unwrap_or_default();
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
    let aggs: Vec<Agg> = order.into_iter().filter_map(|k| map.remove(&k)).collect();
    // Precompute each hit's score once, then sort — don't rebuild haystacks inside
    // the comparator.
    let mut scored: Vec<(f32, SearchHit)> = aggs
        .into_iter()
        .map(|a| (score(&a, terms), a.hit))
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.into_iter().map(|(_, h)| h).collect()
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

/// Pace one engine (sleep so it's >= PACE_MS since its last cross-process
/// request), record the request, then fetch with the per-engine timeout.
async fn fetch_paced(
    client: &reqwest::Client,
    name: &str,
    url: String,
    last_ms: u64,
) -> Option<(u16, String)> {
    let sl = pace_sleep_ms(last_ms, now_ms());
    if sl > 0 {
        tokio::time::sleep(Duration::from_millis(sl)).await;
    }
    record_request(name);
    let fut = async {
        let resp = client.get(&url).send().await.ok()?;
        let status = resp.status().as_u16();
        let body = resp.text().await.ok()?;
        Some((status, body))
    };
    tokio::time::timeout(ENGINE_TIMEOUT, fut)
        .await
        .ok()
        .flatten()
}

/// Run one engine end-to-end: honor cooldown, pace, fetch, classify, and open a
/// cooldown on a block/captcha/429/202. Returns (hits, `<name>:<status>`). When
/// `bypass` is set this engine was force-run despite its cooldown (all engines
/// were cooled); its status is suffixed `(cooldown-bypass)`.
async fn run_engine(
    client: &reqwest::Client,
    name: &'static str,
    url: String,
    parse: fn(&str) -> Vec<SearchHit>,
    cooled: bool,
    bypass: bool,
    last_ms: u64,
) -> (Vec<SearchHit>, String) {
    if cooled {
        return (Vec::new(), format!("{name}:skipped:cooldown"));
    }
    let (hits, status) = classify(fetch_paced(client, name, url, last_ms).await, parse);
    if status.starts_with("blocked") {
        set_cooldown(name);
    }
    let sfx = if bypass { "(cooldown-bypass)" } else { "" };
    (hits, format!("{name}:{status}{sfx}"))
}

pub(crate) async fn search_run(
    query: &str,
    max_results: usize,
    site: Option<&str>,
    exclude_sites: Option<&[String]>,
    freshness: Option<&str>,
) -> anyhow::Result<(Vec<SearchHit>, Vec<String>, Option<String>)> {
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
    let lite_url = format!("https://lite.duckduckgo.com/lite/?q={q}");
    // setlang/cc/form pin Bing to US-English results; without them this IP gets
    // off-topic junk (e.g. the Rust *video game*) for developer queries.
    let bing_url = format!("https://www.bing.com/search?q={q}&setlang=en&cc=US&form=QBLH");
    let mojeek_url = format!("https://www.mojeek.com/search?q={q}");
    let brave_url = format!("https://search.brave.com/search?q={q}&source=web");
    let yahoo_url = format!("https://search.yahoo.com/search?p={q}&n=10");

    // Cross-process pacing/cooldown snapshot: decide skips and sleeps from the
    // shared state file so parallel `scour search` runs don't hammer one engine.
    let states = load_states();
    let now = now_ms();
    // If every engine is in cooldown, force-run the soonest-expiring one(s) so we
    // always make a live attempt instead of bailing "all failed" offline.
    let bypass = cooldown_bypass(&states, now);
    let bypassed = |name: &str| bypass.iter().any(|b| b == name);
    let cooled = |name: &str| {
        states
            .get(name)
            .map(|s| in_cooldown(s, now))
            .unwrap_or(false)
            && !bypassed(name)
    };
    let last_of = |name: &str| states.get(name).map(|s| s.last_ms).unwrap_or(0);

    // DDG: pace + cooldown under the shared "ddg" slot, with a lite fallback when
    // the main html endpoint is blocked/empty/timed out.
    let ddg_bypass = bypassed("ddg");
    let ddg_task = async {
        let sfx = if ddg_bypass { "(cooldown-bypass)" } else { "" };
        if cooled("ddg") {
            return (Vec::new(), "ddg:skipped:cooldown".to_string());
        }
        let (hits, status) = classify(
            fetch_paced(client, "ddg", ddg_url, last_of("ddg")).await,
            parse_ddg,
        );
        if status.starts_with("ok") {
            return (hits, format!("ddg:{status}{sfx}"));
        }
        if status.starts_with("blocked") {
            set_cooldown("ddg");
        }
        // Lite fallback shares the ddg slot; last_ms=0 skips a second sleep.
        let (lhits, lstatus) = classify(
            fetch_paced(client, "ddg", lite_url, 0).await,
            parse_ddg_lite,
        );
        if lstatus.starts_with("ok") {
            (lhits, format!("ddg-lite:{lstatus}{sfx}"))
        } else {
            (hits, format!("ddg:{status}{sfx}"))
        }
    };

    let (ddg_r, bing_r, brave_r, mojeek_r, yahoo_r) = tokio::join!(
        ddg_task,
        run_engine(
            client,
            "bing",
            bing_url,
            parse_bing,
            cooled("bing"),
            bypassed("bing"),
            last_of("bing")
        ),
        run_engine(
            client,
            "brave",
            brave_url,
            parse_brave,
            cooled("brave"),
            bypassed("brave"),
            last_of("brave")
        ),
        run_engine(
            client,
            "mojeek",
            mojeek_url,
            parse_mojeek,
            cooled("mojeek"),
            bypassed("mojeek"),
            last_of("mojeek")
        ),
        run_engine(
            client,
            "yahoo",
            yahoo_url,
            parse_yahoo,
            cooled("yahoo"),
            bypassed("yahoo"),
            last_of("yahoo")
        ),
    );

    let engines = vec![
        ddg_r.1.clone(),
        bing_r.1.clone(),
        brave_r.1.clone(),
        mojeek_r.1.clone(),
        yahoo_r.1.clone(),
    ];
    let ok_count = engines.iter().filter(|e| e.contains(":ok:")).count();
    // Every engine failed (none returned ok) — surface the block, don't lie
    // "no results".
    if ok_count == 0 {
        anyhow::bail!("all search engines failed: [{}]", engines.join(", "));
    }

    let terms = query_terms(query);
    let mut merged = fuse_and_rank(
        vec![ddg_r.0, bing_r.0, brave_r.0, mojeek_r.0, yahoo_r.0],
        &terms,
    );

    if let Some(s) = site
        && !s.trim().is_empty()
    {
        merged.retain(|h| host_matches(&host_of(&h.url), s));
    }
    if let Some(excludes) = exclude_sites {
        let domains: Vec<&String> = excludes.iter().filter(|e| !e.trim().is_empty()).collect();
        if !domains.is_empty() {
            merged.retain(|h| {
                let host = host_of(&h.url);
                !domains.iter().any(|ex| host_matches(&host, ex))
            });
        }
    }

    let warning = low_relevance_warning(&merged, &terms, &engines, ok_count);
    // Drop obvious off-topic noise: when enough hits cover at least one query
    // term, zero-coverage hits are junk from an engine that answered a different
    // question (e.g. Bing returning the Rust video game).
    drop_zero_coverage(&mut merged, &terms);

    merged.truncate(max_results);
    Ok((merged, engines, warning))
}

/// Drop hits with zero query-term coverage, but only when at least 3 hits
/// actually cover something — otherwise a sparse-but-valid result set survives.
fn drop_zero_coverage(merged: &mut Vec<SearchHit>, terms: &[String]) {
    if terms.is_empty() {
        return;
    }
    let positive = merged.iter().filter(|h| coverage(h, terms) > 0.0).count();
    if positive >= 3 {
        merged.retain(|h| coverage(h, terms) > 0.0);
    }
}

/// Low-relevance signal: warn when the best hit covers < half the distinct query
/// terms, or fewer than 2 engines returned results. Returns the human-readable
/// warning string (also surfaced as `<!-- warning: … -->` and in --json).
fn low_relevance_warning(
    merged: &[SearchHit],
    terms: &[String],
    engines: &[String],
    ok_count: usize,
) -> Option<String> {
    if terms.is_empty() {
        return None;
    }
    let best_cov = merged.first().map(|h| coverage(h, terms)).unwrap_or(0.0);
    if best_cov >= 0.5 && ok_count >= 2 {
        return None;
    }
    let found = (best_cov * terms.len() as f32).round() as usize;
    Some(format!(
        "low relevance: top hits cover {}/{} query terms; engines: {} — retry later, rephrase with fewer terms, or fetch known URLs",
        found,
        terms.len(),
        engines.join(","),
    ))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddg_uddg_decode_multibyte() {
        // query_pairs percent-decodes: %C3%A9 = é.
        assert_eq!(
            resolve_ddg_href("//duckduckgo.com/l/?uddg=https%3A%2F%2Fex.com%2Fcaf%C3%A9&rut=x"),
            "https://ex.com/café"
        );
        // root-relative href (no scheme/host) must still resolve.
        assert_eq!(
            resolve_ddg_href("/l/?uddg=https%3A%2F%2Fex.com%2Fpage&rut=x"),
            "https://ex.com/page"
        );
    }

    #[test]
    fn bing_ck_decode() {
        // base64url(no pad) of https://github.com/tokio-rs/axum
        let href =
            "https://www.bing.com/ck/a?!&&p=xx&u=a1aHR0cHM6Ly9naXRodWIuY29tL3Rva2lvLXJzL2F4dW0";
        assert_eq!(decode_bing_href(href), "https://github.com/tokio-rs/axum");
        // non-ck passes through
        assert_eq!(
            decode_bing_href("https://example.com/x"),
            "https://example.com/x"
        );
    }

    #[test]
    fn ddg_fixture_parses() {
        let html = include_str!("../tests/fixtures/ddg.html");
        let hits = parse_ddg(html);
        assert!(hits.len() >= 2, "got {}", hits.len());
        assert!(hits.iter().all(|h| h.url.starts_with("http")));
        assert!(hits.iter().all(|h| !is_junk_url(&h.url)));
        assert!(
            hits.iter().any(|h| !h.snippet.is_empty()),
            "expected snippets"
        );
    }

    #[test]
    fn bing_fixture_parses_and_decodes() {
        let html = include_str!("../tests/fixtures/bing.html");
        let hits = parse_bing(html);
        assert!(hits.len() >= 2, "got {}", hits.len());
        assert!(
            hits.iter().all(|h| !h.url.contains("bing.com")),
            "ck/a not decoded"
        );
        assert!(
            hits.iter().any(|h| !h.snippet.is_empty()),
            "expected snippets"
        );
    }

    #[test]
    fn brave_fixture_parses() {
        let html = include_str!("../tests/fixtures/brave.html");
        let hits = parse_brave(html);
        assert!(hits.len() >= 4, "got {}", hits.len());
        assert!(hits.iter().all(|h| h.url.starts_with("http")));
        assert!(
            hits.iter().all(|h| !h.url.contains("brave.com")),
            "internal brave link leaked"
        );
        assert_eq!(
            hits[0].url,
            "https://docs.rs/tokio/latest/tokio/macro.select.html"
        );
        assert!(
            hits.iter().any(|h| !h.snippet.is_empty()),
            "expected snippets"
        );
    }

    #[test]
    fn ddg_lite_fixture_parses() {
        let html = include_str!("../tests/fixtures/ddglite.html");
        let hits = parse_ddg_lite(html);
        assert_eq!(hits.len(), 3, "got {}", hits.len());
        // uddg redirect must be decoded to the real target.
        assert_eq!(
            hits[0].url,
            "https://docs.rs/tokio/latest/tokio/macro.select.html"
        );
        assert!(
            hits.iter().all(|h| !h.url.contains("duckduckgo.com")),
            "redirect not resolved"
        );
        assert!(
            hits.iter().all(|h| !h.snippet.is_empty()),
            "expected snippets"
        );
    }

    #[test]
    fn ddg_lite_missing_snippet_stays_aligned() {
        // 2nd result has no snippet row; its empty snippet must not shift the 3rd
        // result's snippet onto it.
        let html = include_str!("../tests/fixtures/ddglite_nosnippet.html");
        let hits = parse_ddg_lite(html);
        assert_eq!(hits.len(), 3, "got {}", hits.len());
        assert_eq!(
            hits[0].url,
            "https://docs.rs/tokio/latest/tokio/macro.select.html"
        );
        assert!(
            hits[0].snippet.contains("aggregates"),
            "got {:?}",
            hits[0].snippet
        );
        assert_eq!(hits[1].url, "https://tokio.rs/tokio/tutorial/select");
        assert!(
            hits[1].snippet.is_empty(),
            "2nd result should have no snippet, got {:?}",
            hits[1].snippet
        );
        assert_eq!(hits[2].url, "https://github.com/tokio-rs/tokio");
        assert!(
            hits[2].snippet.contains("reliable"),
            "got {:?}",
            hits[2].snippet
        );
    }

    #[test]
    fn host_matches_suffix_not_substring() {
        assert!(host_matches("example.com", "example.com"));
        assert!(host_matches("a.example.com", "example.com"));
        assert!(host_matches("example.com", "www.example.com"));
        assert!(host_matches("example.com", ".example.com"));
        // substring match must NOT trigger
        assert!(!host_matches("notexample.com", "example.com"));
        assert!(!host_matches("example.com.evil.com", "example.com"));
        assert!(!host_matches("", "example.com"));
        assert!(!host_matches("example.com", ""));
    }

    #[test]
    fn block_detection() {
        let blocked = include_str!("../tests/fixtures/ddglite_blocked.html");
        assert_eq!(detect_block(blocked), Some("anomaly"));
        // Real result pages are not flagged.
        assert_eq!(
            detect_block(include_str!("../tests/fixtures/brave.html")),
            None
        );
        assert_eq!(
            detect_block(include_str!("../tests/fixtures/ddglite.html")),
            None
        );
        // Status-based classification.
        assert_eq!(classify(None, parse_brave).1, "timeout");
        assert_eq!(
            classify(Some((202, String::new())), parse_brave).1,
            "blocked:http202"
        );
        assert_eq!(
            classify(Some((200, blocked.to_string())), parse_ddg_lite).1,
            "blocked:anomaly"
        );
        assert_eq!(
            classify(Some((200, "<html></html>".into())), parse_brave).1,
            "empty"
        );
        let (hits, st) = classify(
            Some((
                200,
                include_str!("../tests/fixtures/ddglite.html").to_string(),
            )),
            parse_ddg_lite,
        );
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
            vec![
                vec![wiki.clone()],
                vec![wiki.clone()],
                vec![wiki.clone(), compare.clone()],
            ],
            &terms,
        );
        assert_eq!(out[0].url, compare.url, "full-coverage hit must rank first");
    }

    #[test]
    fn pacing_math() {
        // No prior request ⇒ no sleep.
        assert_eq!(pace_sleep_ms(0, 10_000), 0);
        // Enough time already elapsed ⇒ no sleep.
        assert_eq!(pace_sleep_ms(1_000, 1_000 + PACE_MS), 0);
        assert_eq!(pace_sleep_ms(1_000, 10_000), 0);
        // Clock went backwards ⇒ no sleep (never negative/underflow).
        assert_eq!(pace_sleep_ms(10_000, 1_000), 0);
        // Partway through the window ⇒ sleep the remainder.
        assert_eq!(pace_sleep_ms(1_000, 1_100), PACE_MS - 100);
        // Never sleep longer than the cap even with a tiny gap.
        assert_eq!(
            pace_sleep_ms(1_000, 1_001),
            PACE_MAX_SLEEP_MS.min(PACE_MS - 1)
        );
    }

    #[test]
    fn cooldown_skip_and_duration() {
        let now = 1_000_000;
        let active = EngineState {
            last_ms: 0,
            cooldown_until_ms: now + 5_000,
        };
        let expired = EngineState {
            last_ms: 0,
            cooldown_until_ms: now - 1,
        };
        assert!(in_cooldown(&active, now));
        assert!(!in_cooldown(&expired, now));
        assert!(!in_cooldown(&EngineState::default(), now));
        // DDG cools down far longer than the others.
        assert_eq!(cooldown_ms_for("ddg"), COOLDOWN_DDG_MS);
        assert_eq!(cooldown_ms_for("ddg-lite"), COOLDOWN_DDG_MS);
        assert_eq!(cooldown_ms_for("bing"), COOLDOWN_OTHER_MS);
        assert!(COOLDOWN_DDG_MS > COOLDOWN_OTHER_MS);
    }

    #[test]
    fn cooldown_bypass_picks_soonest() {
        let now = 1_000_000;
        let mk = |until: u64| EngineState {
            last_ms: 0,
            cooldown_until_ms: until,
        };
        let mut states = EngineStates::new();
        states.insert("ddg".into(), mk(now + 9_000));
        states.insert("bing".into(), mk(now + 1_000));
        states.insert("brave".into(), mk(now + 2_000));
        states.insert("mojeek".into(), mk(now + 8_000));
        states.insert("yahoo".into(), mk(now + 5_000));
        // All cooled ⇒ force-run the 2 soonest-expiring.
        assert_eq!(
            cooldown_bypass(&states, now),
            vec!["bing".to_string(), "brave".to_string()]
        );
        // One engine already available ⇒ no bypass.
        states.insert("yahoo".into(), mk(now - 1));
        assert!(cooldown_bypass(&states, now).is_empty());
        // Never-used engines count as available ⇒ no bypass.
        assert!(cooldown_bypass(&EngineStates::new(), now).is_empty());
    }

    #[test]
    fn classify_cooldown_only_on_real_blocks() {
        let parse = |_: &str| Vec::<SearchHit>::new();
        // Real blocks ⇒ `blocked:*` (caller opens a cooldown).
        for s in [202u16, 403, 429] {
            let (_, st) = classify(Some((s, String::new())), parse);
            assert!(st.starts_with("blocked"), "{s} -> {st}");
        }
        // Transient 5xx / other errors ⇒ `error:*`, no cooldown.
        for s in [500u16, 502, 503, 404] {
            let (_, st) = classify(Some((s, String::new())), parse);
            assert_eq!(st, format!("error:http{s}"));
            assert!(!st.starts_with("blocked"));
        }
        // Timeout never cools down.
        assert_eq!(classify(None, parse).1, "timeout");
    }

    #[test]
    fn percent_decode_handles_multibyte_and_bad_hex() {
        // Normal escapes decode.
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("caf%C3%A9"), "café");
        // Multibyte char right after '%' must not panic; '%' kept literally.
        assert_eq!(percent_decode("%\u{20ac}"), "%\u{20ac}");
        // Invalid hex after '%' kept literally.
        assert_eq!(percent_decode("100%zz"), "100%zz");
        // Trailing '%' with no room to decode.
        assert_eq!(percent_decode("abc%"), "abc%");
    }

    #[test]
    fn yahoo_fixture_parses_and_decodes_ru() {
        let html = include_str!("../tests/fixtures/yahoo.html");
        let hits = parse_yahoo(html);
        assert!(hits.len() >= 3, "got {}", hits.len());
        // RU= redirect decoded to the real target, no yahoo redirect host leaks.
        assert!(hits.iter().all(|h| h.url.starts_with("http")));
        assert!(
            hits.iter().all(|h| !h.url.contains("search.yahoo.com")),
            "RU not decoded"
        );
        assert!(
            hits.iter().any(|h| !h.snippet.is_empty()),
            "expected snippets"
        );
        assert!(
            hits.iter().any(|h| h.title.to_lowercase().contains("axum")),
            "expected on-topic titles"
        );
    }

    #[test]
    fn yahoo_ru_decode_unit() {
        let href = "https://r.search.yahoo.com/_ylt=A0;_ylu=X3/RU=https%3a%2f%2fgithub.com%2ftokio-rs%2faxum/RK=2/RS=abc";
        assert_eq!(decode_yahoo_href(href), "https://github.com/tokio-rs/axum");
        // non-redirect passes through unchanged.
        assert_eq!(
            decode_yahoo_href("https://example.com/x"),
            "https://example.com/x"
        );
    }

    #[test]
    fn low_relevance_warning_fires() {
        let terms = query_terms("axum vs actix web 2025"); // axum, actix, web
        // Top hit covers only 1/3 terms ⇒ warn, and report 1/3.
        let weak = vec![SearchHit {
            title: "Axum town".into(),
            url: "https://x.com/a".into(),
            snippet: "ethiopia".into(),
        }];
        let engines = vec!["bing:ok:7".into(), "yahoo:ok:7".into()];
        let w = low_relevance_warning(&weak, &terms, &engines, 2).expect("warn");
        assert!(w.contains("1/3"), "{w}");
        assert!(w.contains("bing:ok:7,yahoo:ok:7"), "{w}");
        // Strong coverage + 2 ok engines ⇒ no warning.
        let strong = vec![SearchHit {
            title: "axum vs actix web".into(),
            url: "https://x.com/b".into(),
            snippet: "".into(),
        }];
        assert!(low_relevance_warning(&strong, &terms, &engines, 2).is_none());
        // Good coverage but only 1 ok engine ⇒ warn.
        assert!(low_relevance_warning(&strong, &terms, &engines, 1).is_some());
    }

    #[test]
    fn zero_coverage_dropped_when_enough_signal() {
        let terms = query_terms("axum actix web");
        let mk = |t: &str| SearchHit {
            title: t.into(),
            url: "https://x.com/".to_string() + t,
            snippet: String::new(),
        };
        let mut hits = vec![
            mk("axum"),
            mk("actix"),
            mk("web"),
            mk("unrelated video game"),
        ];
        drop_zero_coverage(&mut hits, &terms);
        assert_eq!(hits.len(), 3, "zero-coverage junk should be dropped");
        assert!(hits.iter().all(|h| coverage(h, &terms) > 0.0));
        // Too few positive hits ⇒ keep everything (don't nuke a sparse set).
        let mut sparse = vec![mk("axum"), mk("nothing here")];
        drop_zero_coverage(&mut sparse, &terms);
        assert_eq!(sparse.len(), 2);
    }

    #[test]
    fn social_hosts_demoted() {
        let terms = query_terms("rust tokio select");
        let article = SearchHit {
            title: "rust tokio select guide".into(),
            url: "https://docs.rs/tokio".into(),
            snippet: "select macro".into(),
        };
        let yt = SearchHit {
            title: "rust tokio select video".into(),
            url: "https://www.youtube.com/watch?v=x".into(),
            snippet: "select macro".into(),
        };
        // Same coverage & RRF; youtube demoted ⇒ article first.
        let out = fuse_and_rank(vec![vec![yt.clone()], vec![article.clone()]], &terms);
        assert_eq!(out[0].url, article.url);
    }
}
