//! The scrape pipeline: validation, the HTTP→Chrome ladder, focus/truncation,
//! PDF handling, media extraction, browser actions, crawl, links, screenshot,
//! and per-fetch stats.

use crate::cache::{cache_get, cache_key, cache_key_ext, cache_set};
use crate::cli::fetch_all;
use crate::types::*;
use crate::{extractor, fetcher, tools};
use std::collections::{HashSet, VecDeque};
use std::net::IpAddr;
use std::sync::OnceLock;
use std::time::Duration;

/// Parameters for the scrape pipeline. Build with `ScrapeParams::new(url, max_chars)`
/// then override the fields that differ from the defaults.
#[derive(Clone, Copy)]
pub(crate) struct ScrapeParams<'a> {
    pub url: &'a str,
    pub focus: Option<&'a str>,
    pub max_chars: usize,
    pub include_links: bool,
    pub offset: Option<usize>,
    pub include_media: bool,
    pub pages: Option<&'a str>,
    pub actions: Option<&'a [Action]>,
}

impl<'a> ScrapeParams<'a> {
    pub fn new(url: &'a str, max_chars: usize) -> Self {
        Self {
            url,
            focus: None,
            max_chars,
            include_links: true,
            offset: None,
            include_media: false,
            pages: None,
            actions: None,
        }
    }
}

/// Parameters for `crawl_run`.
#[derive(Clone, Copy)]
pub(crate) struct CrawlParams<'a> {
    pub start_url: &'a str,
    pub max_pages: usize,
    pub max_depth: usize,
    pub focus: Option<&'a str>,
    pub max_chars: usize,
    pub sitemap: Option<bool>,
    pub discover_only: Option<bool>,
    pub max_total_chars: Option<usize>,
}

/// Timing/size metrics for a scrape, passed to `Stats::scrape`.
#[derive(Default)]
struct ScrapeMetrics {
    cache: &'static str,
    status: u16,
    ms: u128,
    ms_fetch: u128,
    ms_parse: u128,
    bytes_down: usize,
    dropped_paras: usize,
}

#[derive(serde::Serialize)]
pub(crate) struct Stats {
    pub(crate) op: &'static str,
    url: String,
    tier: String,
    status: u16,
    cache: &'static str,
    ms: u128,
    ms_fetch: u128,
    ms_parse: u128,
    bytes_down: usize,
    chars_out: usize,
    est_tokens: usize,
    truncated: bool,
    dropped_paras: usize,
    quality: f32,
    page_type: String,
}

impl Stats {
    fn scrape(r: &ScrapeResult, m: ScrapeMetrics) -> Stats {
        let chars_out = r.markdown.chars().count();
        Stats {
            op: "scrape",
            url: r.url.clone(),
            tier: r.tier.clone(),
            status: m.status,
            cache: m.cache,
            ms: m.ms,
            ms_fetch: m.ms_fetch,
            ms_parse: m.ms_parse,
            bytes_down: m.bytes_down,
            chars_out,
            est_tokens: chars_out / 4,
            truncated: r.truncated,
            dropped_paras: m.dropped_paras,
            quality: r.quality_score,
            page_type: r.page_type.clone(),
        }
    }

    pub(crate) fn print(&self) {
        if let Ok(s) = serde_json::to_string(self) {
            eprintln!("stats: {s}");
        }
    }
}

pub(crate) fn validate(url: &str) -> anyhow::Result<()> {
    let u = url::Url::parse(url)?;
    match u.scheme() {
        "http" | "https" => {}
        s => anyhow::bail!("refusing non-http scheme: {s}"),
    }
    // Literal IPs only; hostnames are checked against resolved IPs in fetcher.
    let literal = match u.host() {
        Some(url::Host::Ipv4(v4)) => Some(IpAddr::V4(v4)),
        Some(url::Host::Ipv6(v6)) => Some(IpAddr::V6(v6)),
        _ => None,
    };
    if let Some(ip) = literal
        && !fetcher::is_public_ip(ip)
    {
        anyhow::bail!("refusing non-public IP: {ip}");
    }
    Ok(())
}

/// Appended when a focus query matches nothing, so the unfocused text is not silent.
const FOCUS_MISS: &str = "<!-- focus matched nothing; unfocused page shown -->";

/// Paragraphs split on blank lines, except that a fenced code block is never split: it is one
/// paragraph even when it holds blank lines.
fn paragraphs(md: &str) -> Vec<&str> {
    let pieces: Vec<&str> = md.split("\n\n").collect();
    let last = pieces.len() - 1;
    let mut out = Vec::new();
    let (mut start, mut end) = (0, 0);
    let mut fence = None;
    for (i, piece) in pieces.iter().enumerate() {
        end += piece.len();
        fence = piece.lines().fold(fence, tools::fence_step);
        if fence.is_none() || i == last {
            out.push(&md[start..end]);
            start = end + 2;
        }
        end += 2;
    }
    out
}

/// The paragraphs most relevant to `query`, in page order. `None` when the query has no usable
/// term or nothing matches.
pub(crate) fn focus_filter(markdown: &str, query: &str, keep: usize) -> Option<String> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| {
            t.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|t| t.len() > 2)
        .collect();
    if terms.is_empty() {
        return None;
    }
    let paras = paragraphs(markdown);
    let n = paras.len() as f32;
    let mut df: Vec<usize> = vec![0; terms.len()];
    let lower_paras: Vec<String> = paras.iter().map(|p| p.to_lowercase()).collect();
    for (ti, term) in terms.iter().enumerate() {
        df[ti] = lower_paras.iter().filter(|p| p.contains(term)).count();
    }
    let mut scored: Vec<(f32, usize, &str)> = Vec::new();
    for (i, para) in paras.iter().enumerate() {
        let lower = &lower_paras[i];
        let mut score: f32 = 0.0;
        for (ti, term) in terms.iter().enumerate() {
            let tf = lower.matches(term).count() as f32;
            if tf == 0.0 {
                continue;
            }
            let idf = ((n - df[ti] as f32 + 0.5) / (df[ti] as f32 + 0.5) + 1.0)
                .ln()
                .max(0.1);
            score += tf * idf;
        }
        if score == 0.0 {
            continue;
        }
        let trimmed = para.trim_start();
        if trimmed.starts_with('#') {
            score *= 2.0;
        } else if trimmed.starts_with("##") || trimmed.starts_with("**") {
            score *= 1.5;
        }
        if i < 3 {
            score *= 1.1;
        }
        scored.push((score, i, *para));
    }
    if scored.is_empty() {
        return None;
    }
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.truncate(keep);
    scored.sort_by_key(|(_, i, _)| *i);
    Some(
        scored
            .into_iter()
            .map(|(_, _, p)| p)
            .collect::<Vec<_>>()
            .join("\n\n"),
    )
}

/// The text `offset` and `max_chars` index into, its char count, and whether `focus` matched
/// nothing (then the unfocused text is returned). `focus` applies only when `content_ok`. The
/// caller appends `FOCUS_MISS` after cutting (`cut_page`), so the note is never truncated away.
fn focus_view(markdown: &str, focus: Option<&str>, content_ok: bool) -> (String, usize, bool) {
    let n = markdown.chars().count();
    match focus.filter(|_| content_ok) {
        None => (markdown.to_string(), n, false),
        Some(q) => match focus_filter(markdown, q, 40) {
            Some(text) => {
                let n = text.chars().count();
                (text, n, false)
            }
            None => (markdown.to_string(), n, true),
        },
    }
}

/// Cuts `md` to at most `limit` chars (minimum 1, so paging always advances), preferring a
/// paragraph break. Returns the kept text and whether anything was cut.
pub(crate) fn truncate(md: &str, limit: usize) -> (String, bool) {
    let Some((end, _)) = md.char_indices().nth(limit.max(1)) else {
        return (md.to_string(), false);
    };
    // `end` is a char boundary, so the slice never splits a codepoint.
    let cut = md[..end]
        .rfind("\n\n")
        .filter(|b| *b > end / 2)
        .unwrap_or(end);
    (md[..cut].to_string(), true)
}

/// Closes a fenced code block that a cut left open, so the kept markdown stays balanced.
fn close_fence(md: &str) -> String {
    match md.lines().fold(None, tools::fence_step) {
        Some((c, n)) => format!("{}\n{}", md.trim_end_matches('\n'), c.to_string().repeat(n)),
        None => md.to_string(),
    }
}

/// Cuts one page of output (`text` already sliced at `offset`) to `max_chars`. Returns the
/// markdown and the next offset when cut. A cut inside a code fence is closed, and the focus-miss
/// note goes last, after any cut, so it always shows.
pub(crate) fn cut_page(
    text: &str,
    missed: bool,
    offset: usize,
    max_chars: usize,
) -> (String, Option<usize>) {
    let (cut, truncated) = truncate(text, max_chars);
    let next_offset = truncated.then(|| offset + cut.chars().count());
    let mut md = if truncated { close_fence(&cut) } else { cut };
    if missed {
        md = format!("{md}\n\n{FOCUS_MISS}");
    }
    (md, next_offset)
}

fn extract_headings(markdown: &str) -> Vec<String> {
    markdown
        .lines()
        .filter(|l| l.trim_start().starts_with('#'))
        .map(|l| l.trim().to_string())
        .take(20)
        .collect()
}

/// `s` from char `offset` on (empty when `offset` is past the end).
pub(crate) fn slice_at_offset(s: &str, offset: usize) -> String {
    match s.char_indices().nth(offset) {
        Some((byte, _)) => s[byte..].to_string(),
        None => String::new(),
    }
}

static IMG_SEL: OnceLock<scraper::Selector> = OnceLock::new();
fn img_sel() -> &'static scraper::Selector {
    IMG_SEL.get_or_init(|| scraper::Selector::parse("img[src]").unwrap())
}

fn extract_media(html: &str, base_url: &str) -> Vec<String> {
    let doc = scraper::Html::parse_document(html);
    let sel = img_sel();
    let base = url::Url::parse(base_url).ok();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for el in doc.select(sel) {
        if let Some(src) = el.value().attr("src") {
            let src = src.trim();
            if src.is_empty() || src.starts_with("data:") {
                continue;
            }
            let abs = if let Some(b) = &base {
                b.join(src)
                    .map(|u| u.to_string())
                    .unwrap_or_else(|_| src.to_string())
            } else {
                src.to_string()
            };
            if abs.len() > 2048 {
                continue;
            }
            if seen.insert(abs.clone()) {
                out.push(abs);
                if out.len() >= 50 {
                    break;
                }
            }
        }
    }
    out
}

/// Largest PDF body we read.
const PDF_MAX_BYTES: usize = 20 * 1024 * 1024;

/// Path ends in `.pdf` (query and fragment ignored).
fn is_pdf_url(url: &str) -> bool {
    let path = url.split(&['?', '#'][..]).next().unwrap_or("");
    path.to_ascii_lowercase().ends_with(".pdf")
}

/// Body of `url` if it is a PDF, read directly and capped at `PDF_MAX_BYTES`.
/// `None` for non-PDF bodies and network errors, so the normal ladder takes over.
async fn fetch_pdf_direct(url: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let Ok(mut resp) = fetcher::client().get(url).send().await else {
        return Ok(None);
    };
    let mut body = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                body.extend_from_slice(&chunk);
                if body.len() > PDF_MAX_BYTES {
                    anyhow::bail!("PDF exceeds {} MB cap", PDF_MAX_BYTES / (1024 * 1024));
                }
            }
            Ok(None) => break,
            Err(_) => return Ok(None),
        }
    }
    Ok(body.starts_with(b"%PDF-").then_some(body))
}

/// Text layer of a PDF via `pdftotext` (poppler), if installed and it succeeds.
async fn pdf_text(bytes: Vec<u8>) -> Option<String> {
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut child = std::process::Command::new("pdftotext")
            .args(["-enc", "UTF-8", "-", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        let mut stdin = child.stdin.take()?;
        let feeder = std::thread::spawn(move || stdin.write_all(&bytes));
        let out = child.wait_with_output().ok()?;
        let _ = feeder.join();
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    })
    .await
    .ok()
    .flatten()
}

/// Result for a PDF body: its text layer, or `content_ok: false` when none could be read.
async fn pdf_result(
    url: &str,
    bytes: Vec<u8>,
    pages: Option<&str>,
    focus: Option<&str>,
    max_chars: usize,
    offset: Option<usize>,
) -> ScrapeResult {
    let size = bytes.len();
    let text = pdf_text(bytes).await.unwrap_or_default();
    let text = text.trim();
    let total_chars = text.chars().count();
    let content_ok = total_chars > 20;
    let (focused, focused_chars, missed) = focus_view(text, focus, content_ok);
    let off = offset.unwrap_or(0);
    let (markdown, next_offset) = cut_page(&slice_at_offset(&focused, off), missed, off, max_chars);
    let truncated = next_offset.is_some();
    let (note, next_action) = if content_ok {
        (
            pages.map(|p| format!("pages={p} not applied; whole PDF text returned")),
            None,
        )
    } else {
        (
            Some(format!(
                "no text extracted from PDF ({size} bytes): pdftotext missing, or scanned/image-only PDF"
            )),
            Some(
                "install poppler-utils (pdftotext) or use an HTML version of this document"
                    .to_string(),
            ),
        )
    };
    ScrapeResult {
        url: url.to_string(),
        title: url.to_string(),
        markdown,
        clean_html: String::new(),
        tier: "http".to_string(),
        content_ok,
        blocked: false,
        note,
        next_action,
        total_chars,
        truncated,
        page_type: "media".to_string(),
        quality_score: if content_ok { 0.6 } else { 0.2 },
        description: None,
        links: vec![],
        headings: vec![],
        next_offset,
        media: vec![],
        focused_chars,
    }
}

/// Caches (when `content_ok`) and times a PDF result; shared by the `.pdf` path and magic-detected bodies.
async fn pdf_out(
    key: String,
    bytes: Vec<u8>,
    p: &ScrapeParams<'_>,
    t0: std::time::Instant,
) -> (ScrapeResult, Stats) {
    let pdf = pdf_result(p.url, bytes, p.pages, p.focus, p.max_chars, p.offset).await;
    // A failed read (no pdftotext, scanned PDF) is not cached, so it is retried.
    if pdf.content_ok {
        cache_set(key, pdf.clone());
    }
    let ms = t0.elapsed().as_millis();
    let stats = Stats::scrape(
        &pdf,
        ScrapeMetrics {
            cache: "miss",
            status: 200,
            ms,
            ms_fetch: ms,
            ..Default::default()
        },
    );
    (pdf, stats)
}

async fn fetch_chrome_with_actions(
    url: &str,
    actions: &[Action],
) -> anyhow::Result<fetcher::Fetched> {
    fetcher::check_public_url(url).await?;
    let url_owned = url.to_string();
    let acts = actions.to_owned();
    let fetched =
        tokio::task::spawn_blocking(move || -> anyhow::Result<(String, fetcher::Fetched)> {
            let browser = fetcher::get_browser()?;
            let tab = {
                let guard = browser
                    .lock()
                    .map_err(|_| anyhow::anyhow!("browser lock"))?;
                guard.new_tab()?
            };
            tab.set_default_timeout(Duration::from_secs(30));
            tab.navigate_to(&url_owned)?;
            tab.wait_until_navigated()?;
            std::thread::sleep(Duration::from_millis(700));
            for act in &acts {
                match act {
                    Action::Click(sel) => {
                        if let Ok(el) =
                            tab.wait_for_element_with_custom_timeout(sel, Duration::from_secs(5))
                        {
                            let _ = el.click();
                        }
                    }
                    Action::Fill { selector, text } => {
                        if let Ok(el) = tab
                            .wait_for_element_with_custom_timeout(selector, Duration::from_secs(5))
                        {
                            let _ = el.click();
                            let _ = el.type_into(text);
                        }
                    }
                    Action::Press(key) => {
                        let _ = tab.press_key(key);
                    }
                    Action::Wait(ms) => {
                        std::thread::sleep(Duration::from_millis(*ms));
                    }
                    Action::Scroll(px) => {
                        let expr = format!("window.scrollBy(0,{})", px);
                        let _ = tab.evaluate(&expr, false);
                        std::thread::sleep(Duration::from_millis(250));
                    }
                }
                std::thread::sleep(Duration::from_millis(300));
            }
            std::thread::sleep(Duration::from_millis(600));
            let html = tab.get_content().unwrap_or_default();
            let final_url = tab.get_url();
            let reason = fetcher::block_reason(200, &html);
            let _ = tab.close(true);
            Ok((
                final_url,
                fetcher::Fetched {
                    html,
                    tier: fetcher::Tier::Chrome,
                    status: 200,
                    blocked: reason.is_some(),
                    reason: reason.clone(),
                    first_reason: None,
                    escalated: true,
                },
            ))
        })
        .await??;
    let (final_url, fetched) = fetched;
    fetcher::check_public_url(&final_url).await?;
    Ok(fetched)
}

// Backwards-compatible entry point (used by batch/crawl/cli)
pub(crate) async fn run(
    url: &str,
    focus: Option<&str>,
    max_chars: usize,
    include_links: bool,
) -> anyhow::Result<ScrapeResult> {
    run_ext(ScrapeParams {
        focus,
        include_links,
        ..ScrapeParams::new(url, max_chars)
    })
    .await
}

pub(crate) async fn run_ext(p: ScrapeParams<'_>) -> anyhow::Result<ScrapeResult> {
    Ok(run_ext_stats(p).await?.0)
}

// Same as `run_ext` but also returns timing/size stats for the `--stats` CLI flag.
pub(crate) async fn run_ext_stats(p: ScrapeParams<'_>) -> anyhow::Result<(ScrapeResult, Stats)> {
    let ScrapeParams {
        url,
        focus,
        max_chars,
        include_links,
        offset,
        include_media,
        pages,
        actions,
    } = p;
    let t0 = std::time::Instant::now();
    validate(url)?;
    let actions_len = actions.map(|a| a.len()).unwrap_or(0);
    // Use extended cache key so pagination/actions don't collide
    let key = cache_key_ext(url, focus, max_chars, offset, include_media, pages, actions);
    let default_opts = offset.is_none() && !include_media && pages.is_none() && actions_len == 0;
    if let Some(cached) = cache_get(&key) {
        let ms = t0.elapsed().as_millis();
        let stats = Stats::scrape(
            &cached,
            ScrapeMetrics {
                cache: "hit",
                status: 200,
                ms,
                ..Default::default()
            },
        );
        return Ok((cached, stats));
    }
    // but also check legacy key for backwards compat when offset etc are default
    if default_opts && let Some(cached) = cache_get(&cache_key(url, focus, max_chars)) {
        let ms = t0.elapsed().as_millis();
        let stats = Stats::scrape(
            &cached,
            ScrapeMetrics {
                cache: "hit",
                status: 200,
                ms,
                ..Default::default()
            },
        );
        return Ok((cached, stats));
    }
    // `.pdf` URLs are read directly (size-capped) instead of through the HTML ladder.
    if is_pdf_url(url)
        && let Some(bytes) = fetch_pdf_direct(url).await?
    {
        return Ok(pdf_out(key, bytes, &p, t0).await);
    }

    let t_fetch = std::time::Instant::now();
    let got = if let Some(acts) = actions {
        if !acts.is_empty() {
            fetch_chrome_with_actions(url, acts).await?
        } else {
            fetcher::fetch(url).await?
        }
    } else {
        fetcher::fetch(url).await?
    };
    let ms_fetch = t_fetch.elapsed().as_millis();
    // A PDF without a .pdf suffix arrives as a `%PDF-` body; re-read it raw for its bytes.
    if got.html.starts_with("%PDF-") {
        let bytes = match fetch_pdf_direct(url).await? {
            Some(b) => b,
            None => got.html.into_bytes(),
        };
        return Ok(pdf_out(key, bytes, &p, t0).await);
    }
    let status = got.status;
    let bytes_down = if matches!(got.tier, fetcher::Tier::Http) {
        got.html.len()
    } else {
        0
    };

    let t_parse = std::time::Instant::now();
    let tier = format!("{:?}", got.tier).to_lowercase();
    let doc = extractor::extract(&got.html, url).unwrap_or(extractor::CleanDoc {
        title: String::new(),
        markdown: String::new(),
        clean_html: String::new(),
        text: String::new(),
        links: vec![],
        description: None,
        page_type: "unknown".to_string(),
        quality_score: 0.0,
    });

    let html_len = got.html.len();
    let md_len = doc.markdown.trim().len();
    let text_len = doc.text.trim().len();
    let thin =
        (md_len < 200 && html_len > 2000) || (text_len < 100 && html_len > 1500) || md_len == 0;
    let stealth_wall = got.escalated && thin && got.first_reason.is_some();
    let (content_ok, note, next_action) = if got.blocked || stealth_wall {
        let why = got
            .reason
            .clone()
            .or_else(|| got.first_reason.clone())
            .unwrap_or_else(|| "blocked".into());
        (
            false,
            Some(format!("bot protection not bypassed (tier {tier}, {why}); no article recovered")),
            Some("site likely uses DataDome/Turnstile/Akamai, which no free tool bypasses - try another path on this domain or a different source".to_string()),
        )
    } else if thin {
        if doc.page_type == "list" && md_len > 100 {
            (true, None, None)
        } else {
            (
                false,
                Some(format!(
                    "no article extracted from {} bytes of HTML (app shell, index, or media page) - type: {}",
                    html_len, doc.page_type
                )),
                Some("page has no prose to extract - try a specific article URL".to_string()),
            )
        }
    } else {
        (true, None, None)
    };
    let (content_ok, note, next_action) =
        if !content_ok && doc.page_type == "list" && doc.links.len() > 10 && md_len > 50 {
            (true, None, None)
        } else {
            (content_ok, note, next_action)
        };

    let total_chars = doc.markdown.chars().count();
    let (focused, focused_chars, missed) = focus_view(&doc.markdown, focus, content_ok);
    // Paragraphs removed by focus_filter: total minus what survived.
    let dropped_paras = if focus.is_some() && content_ok {
        paragraphs(&doc.markdown)
            .len()
            .saturating_sub(paragraphs(&focused).len())
    } else {
        0
    };
    // `offset` indexes the focused text, so the hint needs its length, not the page's.
    let off = offset.unwrap_or(0);
    let (markdown, next_offset) = cut_page(&slice_at_offset(&focused, off), missed, off, max_chars);
    let truncated = next_offset.is_some();

    let links: Vec<LinkOut> = if include_links {
        doc.links
            .into_iter()
            .take(50)
            .map(|l| LinkOut {
                href: l.href,
                text: l.text,
            })
            .collect()
    } else {
        vec![]
    };
    let headings = extract_headings(&markdown);
    let media = if include_media {
        extract_media(&got.html, url)
    } else {
        vec![]
    };

    let out = ScrapeResult {
        url: url.to_string(),
        title: doc.title,
        markdown,
        clean_html: doc.clean_html,
        tier,
        content_ok,
        blocked: got.blocked,
        note,
        next_action,
        total_chars,
        truncated,
        page_type: doc.page_type,
        quality_score: doc.quality_score,
        description: doc.description,
        links,
        headings,
        next_offset,
        media,
        focused_chars,
    };
    let ms_parse = t_parse.elapsed().as_millis();
    // Only usable pages are cached; blocked or failed fetches are retried next time.
    if content_ok {
        cache_set(key.clone(), out.clone());
        // also populate legacy key if default options for backwards compat
        if default_opts {
            cache_set(cache_key(url, focus, max_chars), out.clone());
        }
    }
    let ms = t0.elapsed().as_millis();
    let stats = Stats::scrape(
        &out,
        ScrapeMetrics {
            cache: "miss",
            status,
            ms,
            ms_fetch,
            ms_parse,
            bytes_down,
            dropped_paras,
        },
    );
    Ok((out, stats))
}

async fn fetch_sitemap_urls(domain: &str) -> Vec<String> {
    let client = fetcher::client();
    let mut result = Vec::new();
    for path in &["sitemap.xml", "sitemap_index.xml"] {
        let url = format!("https://{}/{}", domain, path);
        if let Ok(resp) = client.get(&url).send().await
            && resp.status().is_success()
            && let Ok(text) = resp.text().await
        {
            let mut start = 0;
            while let Some(open) = text[start..].find("<loc>") {
                let open_idx = start + open + 5;
                if let Some(close) = text[open_idx..].find("</loc>") {
                    let close_idx = open_idx + close;
                    let loc = text[open_idx..close_idx].trim().to_string();
                    if let Ok(u) = url::Url::parse(&loc)
                        && u.host_str() == Some(domain)
                    {
                        result.push(loc);
                    }
                    start = close_idx + 6;
                } else {
                    break;
                }
            }
        }
    }
    result.sort();
    result.dedup();
    result
}

pub(crate) async fn crawl_run(p: CrawlParams<'_>) -> anyhow::Result<CrawlResult> {
    let CrawlParams {
        start_url,
        max_pages,
        max_depth,
        focus,
        max_chars,
        sitemap,
        discover_only,
        max_total_chars,
    } = p;
    validate(start_url)?;
    let start_domain = url::Url::parse(start_url)?
        .host_str()
        .unwrap_or("")
        .to_string();
    if sitemap.unwrap_or(false) {
        let sitemap_urls = fetch_sitemap_urls(&start_domain).await;
        if !sitemap_urls.is_empty() {
            if discover_only.unwrap_or(false) {
                let visited: Vec<String> = sitemap_urls.into_iter().take(max_pages).collect();
                return Ok(CrawlResult {
                    start_url: start_url.to_string(),
                    pages: vec![],
                    visited,
                    total_chars: 0,
                    failed: vec![],
                });
            }
            let mut pages: Vec<ScrapeResult> = Vec::new();
            let mut failed: Vec<(String, anyhow::Error)> = Vec::new();
            let mut visited: HashSet<String> = HashSet::new();
            let mut total_chars = 0usize;
            for url in sitemap_urls.into_iter().take(max_pages) {
                if let Some(limit) = max_total_chars
                    && total_chars >= limit
                {
                    break;
                }
                let norm = normalize_url(&url);
                if !visited.insert(norm) {
                    continue;
                }
                if pages.len() >= max_pages {
                    break;
                }
                let doc = match run(&url, focus, max_chars, true).await {
                    Ok(d) => d,
                    Err(e) => {
                        failed.push((url, e));
                        continue;
                    }
                };
                total_chars += doc.markdown.chars().count();
                let exceed = max_total_chars.map(|l| total_chars > l).unwrap_or(false);
                pages.push(doc);
                if exceed {
                    break;
                }
            }
            let visited_list: Vec<String> = visited.into_iter().collect();
            return Ok(CrawlResult {
                start_url: start_url.to_string(),
                pages,
                visited: visited_list,
                total_chars,
                failed,
            });
        }
    }
    if discover_only.unwrap_or(false) {
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        queue.push_back((start_url.to_string(), 0));
        visited.insert(normalize_url(start_url));
        let mut total_chars = 0usize;
        while let Some((url, depth)) = queue.pop_front() {
            if visited.len() >= max_pages {
                break;
            }
            if depth > max_depth {
                continue;
            }
            if let Some(limit) = max_total_chars
                && total_chars >= limit
            {
                break;
            }
            let doc = match run(&url, focus, max_chars, true).await {
                Ok(d) => d,
                Err(_) => continue,
            };
            total_chars += doc.markdown.chars().count();
            if let Some(limit) = max_total_chars
                && total_chars > limit
            {
                break;
            }
            if depth < max_depth && visited.len() < max_pages {
                for link in &doc.links {
                    let href = &link.href;
                    if visited.contains(&normalize_url(href)) {
                        continue;
                    }
                    if let Ok(u) = url::Url::parse(href) {
                        if u.host_str() != Some(&start_domain) {
                            continue;
                        }
                        if !matches!(u.scheme(), "http" | "https") {
                            continue;
                        }
                        if href.ends_with(".pdf")
                            || href.ends_with(".jpg")
                            || href.ends_with(".png")
                            || href.contains('#')
                        {
                            continue;
                        }
                        let norm = normalize_url(href);
                        if visited.insert(norm.clone()) {
                            queue.push_back((href.clone(), depth + 1));
                            if visited.len() >= max_pages {
                                break;
                            }
                        }
                    }
                }
            }
        }
        let visited_list: Vec<String> = visited.into_iter().collect();
        return Ok(CrawlResult {
            start_url: start_url.to_string(),
            pages: vec![],
            visited: visited_list,
            total_chars: 0,
            failed: vec![],
        });
    }
    // BFS one level at a time, 4 fetches in flight. `visited` lists only pages actually
    // fetched; `queued` stops a URL from being scheduled twice.
    let mut queued: HashSet<String> = HashSet::from([normalize_url(start_url)]);
    let mut visited: Vec<String> = Vec::new();
    let mut pages: Vec<ScrapeResult> = Vec::new();
    let mut failed: Vec<(String, anyhow::Error)> = Vec::new();
    let mut total_chars = 0usize;
    let mut frontier = vec![start_url.to_string()];
    let mut depth = 0usize;
    'bfs: while !frontier.is_empty() {
        let mut next: Vec<String> = Vec::new();
        for chunk in frontier.chunks(4) {
            if let Some(limit) = max_total_chars
                && total_chars >= limit
            {
                break 'bfs;
            }
            for (url, res) in fetch_all(
                chunk.to_vec(),
                focus.map(str::to_string),
                max_chars,
                4,
                "crawl_page",
            )
            .await
            {
                let doc = match res {
                    Ok((doc, _)) => doc,
                    Err(e) => {
                        failed.push((url, e));
                        continue;
                    }
                };
                visited.push(url);
                total_chars += doc.markdown.chars().count();
                let exceed = max_total_chars.is_some_and(|l| total_chars > l);
                if depth < max_depth && !exceed {
                    let mut candidates: Vec<(f32, String)> = Vec::new();
                    for link in &doc.links {
                        let href = &link.href;
                        let Ok(u) = url::Url::parse(href) else {
                            continue;
                        };
                        if u.host_str() != Some(&start_domain)
                            || !matches!(u.scheme(), "http" | "https")
                        {
                            continue;
                        }
                        if href.ends_with(".pdf")
                            || href.ends_with(".jpg")
                            || href.ends_with(".png")
                            || href.contains('#')
                        {
                            continue;
                        }
                        if queued.contains(&normalize_url(href)) {
                            continue;
                        }
                        let score = if let Some(q) = focus {
                            let q_terms: Vec<String> = q
                                .to_lowercase()
                                .split_whitespace()
                                .map(|s| s.to_string())
                                .collect();
                            let lower = link.text.to_lowercase();
                            q_terms.iter().filter(|t| lower.contains(*t)).count() as f32
                        } else {
                            0.0
                        };
                        candidates.push((score, href.clone()));
                    }
                    candidates.sort_by(|a, b| b.0.total_cmp(&a.0));
                    for (_, href) in candidates.into_iter().take(10) {
                        if queued.insert(normalize_url(&href)) {
                            next.push(href);
                        }
                    }
                }
                pages.push(doc);
                if exceed || pages.len() >= max_pages {
                    break 'bfs;
                }
            }
        }
        frontier = next;
        depth += 1;
    }
    Ok(CrawlResult {
        start_url: start_url.to_string(),
        pages,
        visited,
        total_chars,
        failed,
    })
}

pub(crate) fn normalize_url(u: &str) -> String {
    if let Ok(mut parsed) = url::Url::parse(u) {
        parsed.set_fragment(None);
        let mut s = parsed.to_string();
        if s.ends_with('/') && s.len() > 8 {
            s.pop();
        }
        s
    } else {
        u.to_string()
    }
}

/// Raw PNG bytes of `url`, rendered by the headless browser.
pub(crate) async fn screenshot_png(url: &str) -> anyhow::Result<Vec<u8>> {
    validate(url)?;
    fetcher::check_public_url(url).await?;
    let fetcher_browser = fetcher::get_browser()?;
    let tab = {
        let guard = fetcher_browser
            .lock()
            .map_err(|_| anyhow::anyhow!("browser lock"))?;
        guard.new_tab()?
    };
    tab.set_default_timeout(std::time::Duration::from_secs(30));
    tab.navigate_to(url)?;
    tab.wait_until_navigated()?;
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let final_url = tab.get_url();
    if let Err(e) = fetcher::check_public_url(&final_url).await {
        let _ = tab.close(true);
        return Err(e);
    }
    let png = tab.capture_screenshot(
        headless_chrome::protocol::cdp::Page::CaptureScreenshotFormatOption::Png,
        None,
        None,
        true,
    )?;
    let _ = tab.close(true);
    Ok(png)
}

/// Base64 PNG for the MCP tool (truncated preview).
pub(crate) async fn screenshot_run(url: &str) -> anyhow::Result<String> {
    let png = screenshot_png(url).await?;
    use base64::{Engine as _, engine::general_purpose};
    let b64 = general_purpose::STANDARD.encode(&png);
    Ok(if b64.len() > 20000 {
        format!("{}...[truncated {} bytes]", &b64[..20000], png.len())
    } else {
        b64
    })
}

/// Every `<a href>` on the page as absolute http(s) URLs, fragments stripped and deduped
/// (first text wins). Uncapped and unfiltered (unlike `extractor::extract`).
pub(crate) fn page_links(html: &str, base_url: &str) -> Vec<extractor::Link> {
    let doc = scraper::Html::parse_document(html);
    let sel = scraper::Selector::parse("a[href]").unwrap();
    let base = url::Url::parse(base_url).ok();
    let mut seen: HashSet<String> = HashSet::new();
    doc.select(&sel)
        .filter_map(|el| {
            let href = el.value().attr("href")?.trim();
            if href.is_empty() || href.starts_with('#') {
                return None;
            }
            let mut u = match &base {
                Some(b) => b.join(href).ok()?,
                None => url::Url::parse(href).ok()?,
            };
            if !matches!(u.scheme(), "http" | "https") {
                return None;
            }
            u.set_fragment(None);
            let abs = u.to_string();
            seen.insert(abs.clone()).then(|| extractor::Link {
                href: abs,
                text: el.text().collect::<Vec<_>>().join(" "),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn rejects_local_schemes() {
        assert!(super::validate("https://example.com").is_ok());
        assert!(super::validate("file:///etc/passwd").is_err());
        assert!(super::validate("not a url").is_err());
    }

    #[test]
    fn focus_keeps_relevant_paras_in_order() {
        let md = "about cats and yarn\n\nunrelated filler text\n\nmore cats sleeping";
        let out = super::focus_filter(md, "cats", 40).unwrap();
        assert!(out.contains("cats and yarn") && out.contains("more cats"));
        assert!(!out.contains("unrelated filler"));
        assert!(out.find("yarn").unwrap() < out.find("sleeping").unwrap());
        assert_eq!(super::focus_filter(md, "zebra", 40), None);
    }

    #[test]
    fn paragraphs_never_split_a_fence() {
        let p = super::paragraphs;
        assert_eq!(p("a\n\nb"), vec!["a", "b"]);
        assert_eq!(p(""), vec![""]);
        assert_eq!(
            p("a\n\n```\nx\n\ny\n```\n\nb"),
            vec!["a", "```\nx\n\ny\n```", "b"]
        );
        // Unclosed fence swallows the rest of the page as one paragraph.
        assert_eq!(p("a\n\n```\nx\n\ny"), vec!["a", "```\nx\n\ny"]);
    }

    #[test]
    fn focus_keeps_fenced_block_whole() {
        let md = "intro\n\n```rust\nfn cats() {}\n\nlet x = 1;\n```\n\nmore";
        // keep=1 picks the code block; splitting it on its blank line would leave a lone fence.
        assert_eq!(
            super::focus_filter(md, "cats", 1).unwrap(),
            "```rust\nfn cats() {}\n\nlet x = 1;\n```"
        );
    }

    #[test]
    fn focus_view_counts_page_text_and_flags_miss() {
        let md = "about cats\n\nunrelated filler";
        let (text, n, missed) = super::focus_view(md, Some("cats"), true);
        assert!(!text.contains("unrelated") && !missed);
        assert_eq!(n, text.chars().count());
        let (text, n, missed) = super::focus_view(md, Some("zebra"), true);
        assert!(missed && !text.contains(super::FOCUS_MISS));
        assert_eq!(n, md.chars().count());
        // Focus is ignored for unusable content.
        assert_eq!(
            super::focus_view(md, Some("cats"), false),
            (md.to_string(), md.chars().count(), false)
        );
    }

    #[test]
    fn cut_closes_fence_and_keeps_miss_note_last() {
        let md = "```rust\nlet a = 1;\n\nlet b = 2;\nlet c = 3;\n```";
        // The cut lands inside the fence at the blank line after `let a = 1;`.
        let (out, next) = super::cut_page(md, true, 0, 25);
        assert_eq!(
            out,
            format!("```rust\nlet a = 1;\n```\n\n{}", super::FOCUS_MISS)
        );
        assert_eq!(next, Some(18));
        assert_eq!(out.matches("```").count() % 2, 0);
        // No cut: text untouched, note still appended, no offset.
        assert_eq!(
            super::cut_page("abc", true, 0, 100),
            (format!("abc\n\n{}", super::FOCUS_MISS), None)
        );
        assert_eq!(
            super::cut_page("abc", false, 0, 100),
            ("abc".to_string(), None)
        );
    }

    #[test]
    fn cut_offset_counts_uncut_text() {
        // The offset advances by the kept text only; the closing fence is not page text.
        let md = "```\nxxxxxxxxxx\nyyyyyyyyyy\n```";
        let (out, next) = super::cut_page(md, false, 0, 15);
        assert_eq!(out, "```\nxxxxxxxxxx\n```");
        assert_eq!(next, Some(15));
    }

    #[test]
    fn focus_bm25_heading_boost() {
        let md = "# Cats overview\n\nabout dogs\n\n## Cats sleeping habits\n\nmore dogs";
        let out = super::focus_filter(md, "cats", 2).unwrap();
        assert!(out.contains("Cats overview") || out.contains("Cats sleeping"));
    }

    #[test]
    fn truncate_breaks_on_paragraph() {
        let md = format!("{}\n\n{}", "a".repeat(300), "b".repeat(300));
        let (out, cut) = super::truncate(&md, 400);
        assert!(cut);
        assert!(!out.contains('b'));
        let (whole, cut2) = super::truncate("short", 400);
        assert_eq!((whole.as_str(), cut2), ("short", false));
    }

    #[test]
    fn normalize_strips_fragment() {
        assert_eq!(
            super::normalize_url("https://example.com/a#frag"),
            "https://example.com/a"
        );
        assert_eq!(
            super::normalize_url("https://example.com/b/"),
            "https://example.com/b"
        );
    }

    #[test]
    fn offset_slices_before_truncate() {
        let md = "aaaa\n\nbbbb\n\ncccc\n\ndddd";
        // simulate run logic: slice from offset 6 (should start at bbbb)
        let sliced = super::slice_at_offset(md, 6);
        assert!(sliced.starts_with("bbbb"));
        let (out, truncated) = super::truncate(&sliced, 8);
        assert!(truncated || out.len() <= 8 || out.contains("bbbb"));
    }

    #[test]
    fn next_offset_computed_when_truncated() {
        let md = "a".repeat(500);
        let offset = 100;
        let sliced = super::slice_at_offset(&md, offset);
        let (out, truncated) = super::truncate(&sliced, 50);
        let next = if truncated {
            Some(offset + out.len())
        } else {
            None
        };
        assert_eq!(next, Some(150));
    }

    #[test]
    fn media_extraction_finds_images() {
        let html = r#"<html><body><img src="/a.jpg"><img src="https://example.com/b.png"><img src="data:image/png;base64,xxx"></body></html>"#;
        let media = super::extract_media(html, "https://example.com/page");
        assert!(media.iter().any(|u| u.contains("a.jpg")));
        assert!(media.iter().any(|u| u.contains("b.png")));
        assert!(!media.iter().any(|u| u.contains("data:")));
    }
}
