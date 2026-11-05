//! scour — local MCP scraper + CLI. HTTP first, Chrome only when blocked.
//!
//!   scour            -> MCP server on stdio (for Pi / Claude)
//!   scour <url>      -> print markdown (smoke test, no MCP client needed)
//!   scour --help     -> list every CLI subcommand
//!
//! Frontier upgrade: article + list fallback, links/page_type, BM25 focus,
//! batch parallelism, and in-domain crawl.

mod cache;
mod extractor;
mod fetcher;
mod search;
mod tools;

use cache::{cache_get, cache_key, cache_key_ext, cache_set};
use search::search_run;
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::wrapper::{Json, Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::sync::OnceLock;
use std::time::Duration;

// ── Request / Response types ────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize, schemars::JsonSchema, Clone)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Click(String),
    Fill { selector: String, text: String },
    Press(String),
    Wait(u64),
    Scroll(u32),
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema, Default, Clone)]
pub struct ScrapeOptions {
    #[schemars(description = "Include image URLs (extract <img src> via scraper)")]
    #[serde(default)]
    pub include_media: Option<bool>,
    #[schemars(description = "Browser actions to perform before extraction (escalates to chrome: click, fill, press, wait, scroll)")]
    #[serde(default)]
    pub actions: Option<Vec<Action>>,
    #[schemars(description = "Page range for PDFs like \"1-3\"")]
    #[serde(default)]
    pub pages: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ScrapeRequest {
    #[schemars(description = "Absolute http(s) URL to scrape")]
    pub url: String,
    #[schemars(
        description = "Optional query: return only the paragraphs relevant to it, \
                       best-first. Cuts a huge page down to what you asked about."
    )]
    #[serde(default)]
    pub focus: Option<String>,
    #[schemars(
        description = "Max chars of markdown to return (default 40000). Content is \
                       truncated on a paragraph boundary; see `truncated`/`total_chars`."
    )]
    #[serde(default)]
    pub max_chars: Option<usize>,
    #[schemars(description = "Include extracted links (default true, max 50)")]
    #[serde(default)]
    pub include_links: Option<bool>,
    #[schemars(description = "Pagination offset into markdown for reading subsequent chunks")]
    #[serde(default)]
    pub offset: Option<usize>,
    // deprecated top-level (kept for compat, prefer options.*)
    #[schemars(description = "Deprecated: use options.include_media")]
    #[serde(default)]
    pub include_media: Option<bool>,
    #[schemars(description = "Deprecated: use options.actions")]
    #[serde(default)]
    pub actions: Option<Vec<Action>>,
    #[schemars(description = "Deprecated: use options.pages")]
    #[serde(default)]
    pub pages: Option<String>,
    #[schemars(description = "Advanced bag: include_media, pages, actions")]
    #[serde(default)]
    pub options: Option<ScrapeOptions>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ScrapeBatchRequest {
    #[schemars(description = "List of http(s) URLs to scrape in parallel (max 10)")]
    pub urls: Vec<String>,
    #[schemars(description = "Optional focus query applied to each page")]
    #[serde(default)]
    pub focus: Option<String>,
    #[schemars(description = "Max chars per page (default 40000)")]
    #[serde(default)]
    pub max_chars: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CrawlRequest {
    #[schemars(description = "Start URL - crawl stays on this domain")]
    pub url: String,
    #[schemars(description = "Max pages to fetch (default 5, max 20)")]
    #[serde(default)]
    pub max_pages: Option<usize>,
    #[schemars(description = "Max depth from start (default 2)")]
    #[serde(default)]
    pub max_depth: Option<usize>,
    #[schemars(description = "Optional focus query to prioritize relevant pages")]
    #[serde(default)]
    pub focus: Option<String>,
    #[schemars(description = "Max chars per page (default 8000)")]
    #[serde(default)]
    pub max_chars: Option<usize>,
    #[schemars(description = "If true, fetch sitemap.xml and sitemap_index.xml to seed crawl")]
    #[serde(default)]
    pub sitemap: Option<bool>,
    #[schemars(description = "If true, only discover URLs without fetching page content")]
    #[serde(default)]
    pub discover_only: Option<bool>,
    #[schemars(description = "Stop crawling when total_chars exceeds this limit")]
    #[serde(default)]
    pub max_total_chars: Option<usize>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema, Clone)]
pub struct LinkOut {
    pub href: String,
    pub text: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema, Clone)]
pub struct ScrapeResult {
    pub url: String,
    pub title: String,
    /// Clean article markdown — the field to feed an LLM.
    pub markdown: String,
    /// Sanitized article HTML, for when structure matters.
    pub clean_html: String,
    /// Which rung fetched it: "http" (cheap) or "chrome" (challenge/JS).
    pub tier: String,
    /// THE FIELD TO CHECK FIRST. False means `markdown` is not usable content
    /// (bot wall, empty shell, extraction failure) — read `note` and use
    /// another source rather than treating it as the page.
    pub content_ok: bool,
    /// True when even the browser tier only got a bot-challenge page.
    pub blocked: bool,
    /// Why `content_ok` is false, or a caveat about a thin result. None when
    /// the fetch was clean.
    pub note: Option<String>,
    /// What to do next when `content_ok` is false. Empty when nothing to do.
    pub next_action: Option<String>,
    /// Chars of markdown before any focus/max_chars trimming.
    pub total_chars: usize,
    /// True when `markdown` was cut short by `max_chars`.
    pub truncated: bool,
    /// Page type: article | list | media | unknown
    pub page_type: String,
    /// 0.0-1.0 quality heuristic (text ratio, length, title)
    pub quality_score: f32,
    /// Meta description if present
    pub description: Option<String>,
    /// Extracted links (capped at 50)
    pub links: Vec<LinkOut>,
    /// Headings as table-of-contents
    pub headings: Vec<String>,
    /// Next offset for pagination if truncated
    #[serde(default)]
    #[schemars(description = "Next offset for pagination if truncated")]
    pub next_offset: Option<usize>,
    /// Image URLs if include_media was true
    #[serde(default)]
    #[schemars(description = "Image URLs if include_media")]
    pub media: Vec<String>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct BatchResult {
    pub results: Vec<ScrapeResult>,
    pub total: usize,
    pub ok_count: usize,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct CrawlResult {
    pub start_url: String,
    pub pages: Vec<ScrapeResult>,
    pub visited: Vec<String>,
    pub total_chars: usize,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchRequest {
    #[schemars(description = "Search query")]
    pub query: String,
    #[schemars(description = "Max results (default 10, max 20)")]
    #[serde(default)]
    pub max_results: Option<usize>,
    #[schemars(description = "Limit results to this site (e.g. example.com)")]
    #[serde(default)]
    pub site: Option<String>,
    #[schemars(description = "Exclude these sites")]
    #[serde(default)]
    pub exclude_sites: Option<Vec<String>>,
    #[schemars(description = "Freshness filter: day|week|month|year")]
    #[serde(default)]
    pub freshness: Option<String>,
    /// BYOK stub: accept but ignore extra keys (api keys, etc.)
    #[serde(default, flatten)]
    #[schemars(skip)]
    pub _extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema, Clone)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct SearchResult {
    pub query: String,
    pub hits: Vec<SearchHit>,
    pub total: usize,
    /// Per-engine status, e.g. ["ddg:ok:10","bing:ok:9","brave:blocked:captcha","mojeek:timeout"].
    #[serde(default)]
    pub engines: Vec<String>,
}

// ── MCP Server ──────────────────────────────────────────────────────────────

#[derive(Clone)]
struct Scour {
    #[allow(dead_code)]
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

#[tool_router]
impl Scour {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Scrape a web page and return clean markdown. Handles articles AND list/index pages (HN, Reddit, search). Escalates to a real browser automatically for Cloudflare/JS pages. Use `focus` to get only paragraphs about your query. Supports offset pagination (next_offset). Advanced (options bag): include_media, actions, pages."
    )]
    async fn scrape(
        &self,
        Parameters(req): Parameters<ScrapeRequest>,
    ) -> Result<Json<ScrapeResult>, ErrorData> {
        let opt = req.options.clone().unwrap_or_default();
        let include_media = opt.include_media.or(req.include_media).unwrap_or(false);
        let pages = opt.pages.clone().or(req.pages.clone());
        let actions = opt.actions.clone().or(req.actions.clone());
        let doc = run_ext(
            &req.url,
            req.focus.as_deref(),
            req.max_chars.unwrap_or(40_000),
            req.include_links.unwrap_or(true),
            req.offset,
            include_media,
            pages.as_deref(),
            actions.as_deref(),
        )
        .await
        .map_err(|e| ErrorData::internal_error(format!("scrape failed for {}: {e}", req.url), None))?;
        Ok(Json(doc))
    }

    #[tool(
        description = "Scrape multiple URLs in parallel (up to 10). Same output as `scrape` per URL, but one tool call. Use for comparing sources or fetching a list of article URLs."
    )]
    async fn scrape_batch(
        &self,
        Parameters(req): Parameters<ScrapeBatchRequest>,
    ) -> Result<Json<BatchResult>, ErrorData> {
        if req.urls.is_empty() || req.urls.len() > 10 {
            return Err(ErrorData::invalid_params("urls must be 1..10".to_string(), None));
        }
        let focus = req.focus.clone();
        let max_chars = req.max_chars.unwrap_or(40_000);
        let mut set = tokio::task::JoinSet::new();
        for url in req.urls.clone() {
            let f = focus.clone();
            set.spawn(async move { (url.clone(), run(&url, f.as_deref(), max_chars, true).await) });
        }
        let mut out_map: HashMap<String, ScrapeResult> = HashMap::new();
        let mut ok = 0;
        while let Some(res) = set.join_next().await {
            if let Ok((url, r)) = res {
                match r {
                    Ok(doc) => {
                        if doc.content_ok { ok += 1; }
                        out_map.insert(url, doc);
                    }
                    Err(e) => {
                        out_map.insert(url.clone(), ScrapeResult {
                            url: url.clone(), title: String::new(), markdown: String::new(), clean_html: String::new(),
                            tier: "http".to_string(), content_ok: false, blocked: false,
                            note: Some(format!("fetch failed: {e}")), next_action: Some("try another URL or check connectivity".to_string()),
                            total_chars: 0, truncated: false, page_type: "unknown".to_string(), quality_score: 0.0,
                            description: None, links: vec![], headings: vec![], next_offset: None, media: vec![],
                        });
                    }
                }
            }
        }
        let mut out = Vec::new();
        for url in req.urls.clone() {
            if let Some(doc) = out_map.remove(&url) { out.push(doc); }
        }
        Ok(Json(BatchResult { total: out.len(), ok_count: ok, results: out }))
    }

    #[tool(
        description = "Crawl a site starting from a URL, staying on the same domain. Best-first (focus-aware) BFS. Returns markdown for each page plus the link graph. Use for docs, blogs, or sitemaps without a sitemap.xml."
    )]
    async fn crawl(
        &self,
        Parameters(req): Parameters<CrawlRequest>,
    ) -> Result<Json<CrawlResult>, ErrorData> {
        let max_pages = req.max_pages.unwrap_or(5).clamp(1, 20);
        let max_depth = req.max_depth.unwrap_or(2).min(4);
        let max_chars = req.max_chars.unwrap_or(8000);
        let res = crawl_run(&req.url, max_pages, max_depth, req.focus.as_deref(), max_chars, req.sitemap, req.discover_only, req.max_total_chars)
            .await
            .map_err(|e| ErrorData::internal_error(format!("crawl failed for {}: {e}", req.url), None))?;
        Ok(Json(res))
    }

    #[tool(description = "Clear the in-memory fetch cache (1hr TTL). Use to force a fresh fetch.")]
    async fn cache_clear(&self) -> Result<Json<serde_json::Value>, ErrorData> {
        cache::cache_clear();
        Ok(Json(serde_json::json!({"cleared": true})))
    }

    #[tool(description = "Search the web (keyless, via DuckDuckGo HTML + Bing, parallel, deduped and ranked). Returns title/url/snippet hits. Use before `scrape` to discover URLs. Supports site:, freshness, and exclude_sites filters.")]
    async fn search(
        &self,
        Parameters(req): Parameters<SearchRequest>,
    ) -> Result<Json<SearchResult>, ErrorData> {
        let max = req.max_results.unwrap_or(10).clamp(1, 20);
        let hits = search_run(
            &req.query,
            max,
            req.site.as_deref(),
            req.exclude_sites.as_deref(),
            req.freshness.as_deref(),
        )
        .await
        .map_err(|e| ErrorData::internal_error(format!("search failed: {e}"), None))?;
        Ok(Json(SearchResult {
            query: req.query,
            total: hits.len(),
            hits,
        }))
    }

    #[tool(description = "Screenshot a page via the Brave headless backend. Returns base64 PNG (truncated preview). Requires Chrome/Brave.")]
    async fn screenshot(
        &self,
        Parameters(req): Parameters<ScrapeRequest>,
    ) -> Result<Json<serde_json::Value>, ErrorData> {
        let png = screenshot_run(&req.url).await.map_err(|e| ErrorData::internal_error(format!("screenshot failed for {}: {e}", req.url), None))?;
        Ok(Json(serde_json::json!({"url": req.url, "png_base64": png, "note": "truncated base64, decode to view"})))
    }

    #[tool(description = "Version and build info.")]
    async fn scour_version(&self) -> Result<Json<serde_json::Value>, ErrorData> {
        Ok(Json(version_info()))
    }
}

#[tool_handler]
impl ServerHandler for Scour {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Local AI-first web scraper. Tools: `scrape` (single URL, focus-aware), `scrape_batch` (up to 10 URLs parallel), `crawl` (in-domain BFS). All return clean markdown + links + page_type + content_ok. Check content_ok first.")
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn version_info() -> serde_json::Value {
    serde_json::json!({
        "name": "scour",
        "version": env!("CARGO_PKG_VERSION"),
        "tools": ["scrape", "scrape_batch", "crawl", "search", "screenshot", "cache_clear", "scour_version"],
        "features": ["http+chrome ladder", "readability+fallback", "bm25 focus", "parallel batch", "in-domain crawl", "links+page_type", "1hr cache", "keyless search", "screenshot", "offset pagination", "include_media", "actions", "pdf pages"],
        "browser": std::env::var("SCOUR_BROWSER").unwrap_or("/usr/bin/brave".to_string())
    })
}

/// One stats line per fetch, emitted to stderr as `stats: {json}` under `--stats`.
#[derive(serde::Serialize)]
struct Stats {
    op: &'static str,
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
    #[allow(clippy::too_many_arguments)]
    fn scrape(
        r: &ScrapeResult,
        cache: &'static str,
        status: u16,
        ms: u128,
        ms_fetch: u128,
        ms_parse: u128,
        bytes_down: usize,
        dropped_paras: usize,
    ) -> Stats {
        let chars_out = r.markdown.chars().count();
        Stats {
            op: "scrape",
            url: r.url.clone(),
            tier: r.tier.clone(),
            status,
            cache,
            ms,
            ms_fetch,
            ms_parse,
            bytes_down,
            chars_out,
            est_tokens: chars_out / 4,
            truncated: r.truncated,
            dropped_paras,
            quality: r.quality_score,
            page_type: r.page_type.clone(),
        }
    }

    fn print(&self) {
        if let Ok(s) = serde_json::to_string(self) {
            eprintln!("stats: {s}");
        }
    }
}

fn validate(url: &str) -> anyhow::Result<()> {
    let u = url::Url::parse(url)?;
    match u.scheme() {
        "http" | "https" => {},
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

fn focus_filter(markdown: &str, query: &str, keep: usize) -> String {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
        .filter(|t| t.len() > 2)
        .collect();
    if terms.is_empty() {
        return markdown.to_string();
    }
    let paras: Vec<&str> = markdown.split("\n\n").collect();
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
            if tf == 0.0 { continue; }
            let idf = ((n - df[ti] as f32 + 0.5) / (df[ti] as f32 + 0.5) + 1.0).ln().max(0.1);
            score += tf * idf;
        }
        if score == 0.0 { continue; }
        let trimmed = para.trim_start();
        if trimmed.starts_with('#') {
            score *= 2.0;
        } else if trimmed.starts_with("##") || trimmed.starts_with("**") {
            score *= 1.5;
        }
        if i < 3 { score *= 1.1; }
        scored.push((score, i, *para));
    }
    if scored.is_empty() {
        return markdown.to_string();
    }
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.truncate(keep);
    scored.sort_by_key(|(_, i, _)| *i);
    scored.into_iter().map(|(_, _, p)| p).collect::<Vec<_>>().join("\n\n")
}

fn truncate(md: &str, limit: usize) -> (String, bool) {
    if md.len() <= limit {
        return (md.to_string(), false);
    }
    // `limit` is bytes: back off to a char boundary so we never split a codepoint.
    let mut end = limit;
    while !md.is_char_boundary(end) {
        end -= 1;
    }
    let cut = md[..end]
        .rfind("\n\n")
        .filter(|b| *b > end / 2)
        .unwrap_or(end);
    (md[..cut].to_string(), true)
}

fn extract_headings(markdown: &str) -> Vec<String> {
    markdown
        .lines()
        .filter(|l| l.trim_start().starts_with('#'))
        .map(|l| l.trim().to_string())
        .take(20)
        .collect()
}

fn slice_at_offset(s: &str, offset: usize) -> String {
    if offset == 0 { return s.to_string(); }
    if offset >= s.len() { return String::new(); }
    // ensure char boundary
    if let Some(slice) = s.get(offset..) {
        return slice.to_string();
    }
    let mut idx = offset;
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    if idx < s.len() { s[idx..].to_string() } else { String::new() }
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
            if src.is_empty() || src.starts_with("data:") { continue; }
            let abs = if let Some(b) = &base {
                b.join(src).map(|u| u.to_string()).unwrap_or_else(|_| src.to_string())
            } else {
                src.to_string()
            };
            if abs.len() > 2048 { continue; }
            if seen.insert(abs.clone()) {
                out.push(abs);
                if out.len() >= 50 { break; }
            }
        }
    }
    out
}

async fn try_pdf_fetch(
    url: &str,
    pages: Option<&str>,
    max_chars: usize,
    offset: Option<usize>,
) -> anyhow::Result<Option<ScrapeResult>> {
    let lower = url.to_lowercase();
    let base_no_q = lower.split(&['?', '#'][..]).next().unwrap_or("");
    let url_is_pdf = base_no_q.ends_with(".pdf");
    if !url_is_pdf && pages.is_none() {
        return Ok(None);
    }
    // reuse shared Chrome-UA client (cookie_store enabled) – cheaper than per-call builder
    let client = fetcher::client();
    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(_) => return Ok(None),
    };
    let ct = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_lowercase();
    let status = resp.status().as_u16();
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(_) => return Ok(None),
    };
    let is_pdf_ct = ct.contains("pdf");
    let starts_pdf = bytes.starts_with(b"%PDF");
    // Only handle as PDF if evidence of pdf
    if !is_pdf_ct && !starts_pdf && !url_is_pdf {
        return Ok(None);
    }
    // If status error and not pdf, let normal flow handle
    if status >= 400 && !starts_pdf && !is_pdf_ct {
        return Ok(None);
    }
    let raw = String::from_utf8_lossy(&bytes);
    // first 40000 chars raw as preview
    let preview: String = raw.chars().take(40000).collect();
    let preview_len = preview.len();
    let sliced = slice_at_offset(&preview, offset.unwrap_or(0));
    let (markdown, truncated) = truncate(&sliced, max_chars);
    let next_offset = if truncated {
        Some(offset.unwrap_or(0) + markdown.len())
    } else {
        None
    };
    let note = if starts_pdf || is_pdf_ct {
        Some(format!("PDF extraction requires pdf feature; returning raw text preview ({} bytes, content-type: {}). pages={:?}. Raw preview limited to 40000 chars.", bytes.len(), ct, pages))
    } else if url_is_pdf {
        Some(format!("URL ends with .pdf but Content-Type is {} and header not %PDF; returning raw preview ({} bytes). pages={:?}", ct, bytes.len(), pages))
    } else {
        Some(format!("PDF preview ({} bytes). pages={:?} ", bytes.len(), pages))
    };
    let content_ok = !markdown.trim().is_empty() && markdown.trim().len() > 20;
    let result = ScrapeResult {
        url: url.to_string(),
        title: url.to_string(),
        markdown: markdown.clone(),
        clean_html: String::new(),
        tier: "http".to_string(),
        content_ok,
        blocked: false,
        note,
        next_action: if content_ok { None } else { Some("PDF extraction is minimal without pdf crate; install poppler or enable pdf feature for better results".to_string()) },
        total_chars: preview_len,
        truncated,
        page_type: "media".to_string(),
        quality_score: if content_ok { 0.6 } else { 0.2 },
        description: None,
        links: vec![],
        headings: vec![],
        next_offset,
        media: vec![],
    };
    Ok(Some(result))
}

async fn fetch_chrome_with_actions(url: &str, actions: &[Action]) -> anyhow::Result<fetcher::Fetched> {
    let url_owned = url.to_string();
    let acts = actions.to_owned();
    let fetched = tokio::task::spawn_blocking(move || -> anyhow::Result<fetcher::Fetched> {
        let browser = fetcher::get_browser()?;
        let tab = {
            let guard = browser.lock().map_err(|_| anyhow::anyhow!("browser lock"))?;
            guard.new_tab()?
        };
        tab.set_default_timeout(Duration::from_secs(30));
        tab.navigate_to(&url_owned)?;
        tab.wait_until_navigated()?;
        std::thread::sleep(Duration::from_millis(700));
        for act in &acts {
            match act {
                Action::Click(sel) => {
                    if let Ok(el) = tab.wait_for_element_with_custom_timeout(sel, Duration::from_secs(5)) {
                        let _ = el.click();
                    }
                }
                Action::Fill { selector, text } => {
                    if let Ok(el) = tab.wait_for_element_with_custom_timeout(selector, Duration::from_secs(5)) {
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
        let reason = fetcher::block_reason(200, &html);
        let _ = tab.close(true);
        Ok(fetcher::Fetched {
            html,
            tier: fetcher::Tier::Chrome,
            status: 200,
            blocked: reason.is_some(),
            reason: reason.clone(),
            first_reason: None,
            escalated: true,
            first_status: 200,
        })
    })
    .await??;
    Ok(fetched)
}

// Backwards-compatible entry point (used by batch/crawl/cli)
async fn run(url: &str, focus: Option<&str>, max_chars: usize, include_links: bool) -> anyhow::Result<ScrapeResult> {
    run_ext(url, focus, max_chars, include_links, None, false, None, None).await
}

#[allow(clippy::too_many_arguments)]
async fn run_ext(
    url: &str,
    focus: Option<&str>,
    max_chars: usize,
    include_links: bool,
    offset: Option<usize>,
    include_media: bool,
    pages: Option<&str>,
    actions: Option<&[Action]>,
) -> anyhow::Result<ScrapeResult> {
    Ok(run_ext_stats(url, focus, max_chars, include_links, offset, include_media, pages, actions)
        .await?
        .0)
}

// Same as `run_ext` but also returns timing/size stats for the `--stats` CLI flag.
#[allow(clippy::too_many_arguments)]
async fn run_ext_stats(
    url: &str,
    focus: Option<&str>,
    max_chars: usize,
    include_links: bool,
    offset: Option<usize>,
    include_media: bool,
    pages: Option<&str>,
    actions: Option<&[Action]>,
) -> anyhow::Result<(ScrapeResult, Stats)> {
    let t0 = std::time::Instant::now();
    validate(url)?;
    // PDF early path
    if let Some(pdf) = try_pdf_fetch(url, pages, max_chars, offset).await? {
        // Check if we should still use offset/next_offset and include_media (media empty for pdf)
        // Cache pdf result too
        let key = cache_key_ext(url, focus, max_chars, offset, include_media, pages, actions.map(|a| a.len()).unwrap_or(0));
        // don't cache failed pdf previews? cache anyway
        cache_set(key, pdf.clone());
        let ms = t0.elapsed().as_millis();
        let stats = Stats::scrape(&pdf, "miss", 200, ms, ms, 0, 0, 0);
        return Ok((pdf, stats));
    }

    let actions_len = actions.map(|a| a.len()).unwrap_or(0);
    // Use extended cache key so pagination/actions don't collide
    let key = cache_key_ext(url, focus, max_chars, offset, include_media, pages, actions_len);
    // but also check legacy key for backwards compat when offset etc are default
    if let Some(cached) = cache_get(&key) {
        let ms = t0.elapsed().as_millis();
        let stats = Stats::scrape(&cached, "hit", 200, ms, 0, 0, 0, 0);
        return Ok((cached, stats));
    }
    if offset.is_none() && !include_media && pages.is_none() && actions_len == 0 {
        let legacy = cache_key(url, focus, max_chars);
        if let Some(cached) = cache_get(&legacy) {
            let ms = t0.elapsed().as_millis();
            let stats = Stats::scrape(&cached, "hit", 200, ms, 0, 0, 0, 0);
            return Ok((cached, stats));
        }
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
    let status = got.status;
    let bytes_down = if matches!(got.tier, fetcher::Tier::Http) { got.html.len() } else { 0 };

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
    let thin = (md_len < 200 && html_len > 2000) || (text_len < 100 && html_len > 1500) || md_len == 0;
    let stealth_wall = got.escalated && thin && got.first_reason.is_some();
    let (content_ok, note, next_action) = if got.blocked || stealth_wall {
        let why = got.reason.clone().or_else(|| got.first_reason.clone()).unwrap_or_else(|| "blocked".into());
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
                Some(format!("no article extracted from {} bytes of HTML (app shell, index, or media page) - type: {}", html_len, doc.page_type)),
                Some("page has no prose to extract - try a specific article URL".to_string()),
            )
        }
    } else {
        (true, None, None)
    };
    let (content_ok, note, next_action) = if !content_ok && doc.page_type == "list" && doc.links.len() > 10 && md_len > 50 {
        (true, None, None)
    } else {
        (content_ok, note, next_action)
    };

    let total_chars = doc.markdown.len();
    let focused = match focus {
        Some(q) if content_ok => focus_filter(&doc.markdown, q, 40),
        _ => doc.markdown.clone(),
    };
    // Paragraphs removed by focus_filter: total minus what survived (both split on "\n\n").
    let dropped_paras = if focus.is_some() && content_ok {
        doc.markdown.split("\n\n").count().saturating_sub(focused.split("\n\n").count())
    } else {
        0
    };
    let sliced = slice_at_offset(&focused, offset.unwrap_or(0));
    let (markdown, truncated) = truncate(&sliced, max_chars);
    let next_offset = if truncated {
        Some(offset.unwrap_or(0) + markdown.len())
    } else {
        None
    };

    let links: Vec<LinkOut> = if include_links {
        doc.links.into_iter().take(50).map(|l| LinkOut { href: l.href, text: l.text }).collect()
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
    };
    let ms_parse = t_parse.elapsed().as_millis();
    cache_set(key.clone(), out.clone());
    // also populate legacy key if default options for backwards compat
    if offset.is_none() && !include_media && pages.is_none() && actions_len == 0 {
        cache_set(cache_key(url, focus, max_chars), out.clone());
    }
    let ms = t0.elapsed().as_millis();
    let stats = Stats::scrape(&out, "miss", status, ms, ms_fetch, ms_parse, bytes_down, dropped_paras);
    Ok((out, stats))
}

async fn fetch_sitemap_urls(domain: &str) -> Vec<String> {
    let client = fetcher::client();
    let mut result = Vec::new();
    for path in &["sitemap.xml", "sitemap_index.xml"] {
        let url = format!("https://{}/{}", domain, path);
        if let Ok(resp) = client.get(&url).send().await
            && resp.status().is_success()
                && let Ok(text) = resp.text().await {
                    let mut start = 0;
                    while let Some(open) = text[start..].find("<loc>") {
                        let open_idx = start + open + 5;
                        if let Some(close) = text[open_idx..].find("</loc>") {
                            let close_idx = open_idx + close;
                            let loc = text[open_idx..close_idx].trim().to_string();
                            if let Ok(u) = url::Url::parse(&loc)
                                && u.host_str() == Some(domain) {
                                    result.push(loc);
                                }
                            start = close_idx + 6;
                        } else { break; }
                    }
                }
    }
    result.sort();
    result.dedup();
    result
}

#[allow(clippy::too_many_arguments)]
async fn crawl_run(start_url: &str, max_pages: usize, max_depth: usize, focus: Option<&str>, max_chars: usize, sitemap: Option<bool>, discover_only: Option<bool>, max_total_chars: Option<usize>) -> anyhow::Result<CrawlResult> {
    validate(start_url)?;
    let start_domain = url::Url::parse(start_url)?.host_str().unwrap_or("").to_string();
    if sitemap.unwrap_or(false) {
        let sitemap_urls = fetch_sitemap_urls(&start_domain).await;
        if !sitemap_urls.is_empty() {
            if discover_only.unwrap_or(false) {
                let visited: Vec<String> = sitemap_urls.into_iter().take(max_pages).collect();
                return Ok(CrawlResult { start_url: start_url.to_string(), pages: vec![], visited, total_chars: 0 });
            }
            let mut pages: Vec<ScrapeResult> = Vec::new();
            let mut visited: HashSet<String> = HashSet::new();
            let mut total_chars = 0usize;
            for url in sitemap_urls.into_iter().take(max_pages) {
                if let Some(limit) = max_total_chars
                    && total_chars >= limit { break; }
                let norm = normalize_url(&url);
                if !visited.insert(norm) { continue; }
                if pages.len() >= max_pages { break; }
                let doc = match run(&url, focus, max_chars, true).await {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                total_chars += doc.markdown.len();
                let exceed = max_total_chars.map(|l| total_chars > l).unwrap_or(false);
                pages.push(doc);
                if exceed { break; }
            }
            let visited_list: Vec<String> = visited.into_iter().collect();
            return Ok(CrawlResult { start_url: start_url.to_string(), pages, visited: visited_list, total_chars });
        }
    }
    if discover_only.unwrap_or(false) {
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        queue.push_back((start_url.to_string(), 0));
        visited.insert(normalize_url(start_url));
        let mut total_chars = 0usize;
        while let Some((url, depth)) = queue.pop_front() {
            if visited.len() >= max_pages { break; }
            if depth > max_depth { continue; }
            if let Some(limit) = max_total_chars
                && total_chars >= limit { break; }
            let doc = match run(&url, focus, max_chars, true).await {
                Ok(d) => d,
                Err(_) => continue,
            };
            total_chars += doc.markdown.len();
            if let Some(limit) = max_total_chars
                && total_chars > limit { break; }
            if depth < max_depth && visited.len() < max_pages {
                for link in &doc.links {
                    let href = &link.href;
                    if visited.contains(&normalize_url(href)) { continue; }
                    if let Ok(u) = url::Url::parse(href) {
                        if u.host_str() != Some(&start_domain) { continue; }
                        if !matches!(u.scheme(), "http" | "https") { continue; }
                        if href.ends_with(".pdf") || href.ends_with(".jpg") || href.ends_with(".png") || href.contains('#') { continue; }
                        let norm = normalize_url(href);
                        if visited.insert(norm.clone()) {
                            queue.push_back((href.clone(), depth + 1));
                            if visited.len() >= max_pages { break; }
                        }
                    }
                }
            }
        }
        let visited_list: Vec<String> = visited.into_iter().collect();
        return Ok(CrawlResult { start_url: start_url.to_string(), pages: vec![], visited: visited_list, total_chars: 0 });
    }
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();
    let mut pages: Vec<ScrapeResult> = Vec::new();
    let mut total_chars = 0usize;
    queue.push_back((start_url.to_string(), 0));
    visited.insert(normalize_url(start_url));
    while let Some((url, depth)) = queue.pop_front() {
        if pages.len() >= max_pages { break; }
        if depth > max_depth { continue; }
        if let Some(limit) = max_total_chars
            && total_chars >= limit { break; }
        let res = run(&url, focus, max_chars, true).await;
        let doc = match res {
            Ok(d) => d,
            Err(_) => continue,
        };
        let is_ok = doc.content_ok;
        total_chars += doc.markdown.len();
        let exceed = max_total_chars.map(|l| total_chars > l).unwrap_or(false);
        if depth < max_depth && pages.len() < max_pages && !exceed {
            let mut candidates: Vec<(f32, String)> = Vec::new();
            for link in &doc.links {
                let href = &link.href;
                if visited.contains(&normalize_url(href)) { continue; }
                if let Ok(u) = url::Url::parse(href) {
                    if u.host_str() != Some(&start_domain) { continue; }
                    if !matches!(u.scheme(), "http" | "https") { continue; }
                    if href.ends_with(".pdf") || href.ends_with(".jpg") || href.ends_with(".png") || href.contains('#') {
                        continue;
                    }
                    let norm = normalize_url(href);
                    if visited.contains(&norm) { continue; }
                    let score = if let Some(q) = focus {
                        let q_terms: Vec<String> = q.to_lowercase().split_whitespace().map(|s| s.to_string()).collect();
                        let lower = link.text.to_lowercase();
                        q_terms.iter().filter(|t| lower.contains(*t)).count() as f32
                    } else { 0.0 };
                    candidates.push((score, href.clone()));
                }
            }
            candidates.sort_by(|a, b| b.0.total_cmp(&a.0));
            for (_, href) in candidates.into_iter().take(10) {
                let norm = normalize_url(&href);
                if visited.insert(norm) {
                    queue.push_back((href, depth + 1));
                }
            }
        }
        pages.push(doc);
        if exceed { break; }
        if !is_ok && pages.len() == 1 && max_pages > 1 {
            continue;
        }
    }
    let visited_list: Vec<String> = visited.into_iter().collect();
    Ok(CrawlResult { start_url: start_url.to_string(), pages, visited: visited_list, total_chars })
}

fn normalize_url(u: &str) -> String {
    if let Ok(mut parsed) = url::Url::parse(u) {
        parsed.set_fragment(None);
        let mut s = parsed.to_string();
        if s.ends_with('/') && s.len() > 8 { s.pop(); }
        s
    } else { u.to_string() }
}


/// Raw PNG bytes of `url`, rendered by the headless browser.
async fn screenshot_png(url: &str) -> anyhow::Result<Vec<u8>> {
    validate(url)?;
    let fetcher_browser = fetcher::get_browser()?;
    let tab = {
        let guard = fetcher_browser.lock().map_err(|_| anyhow::anyhow!("browser lock"))?;
        guard.new_tab()?
    };
    tab.set_default_timeout(std::time::Duration::from_secs(30));
    tab.navigate_to(url)?;
    tab.wait_until_navigated()?;
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let png = tab.capture_screenshot(headless_chrome::protocol::cdp::Page::CaptureScreenshotFormatOption::Png, None, None, true)?;
    let _ = tab.close(true);
    Ok(png)
}

/// Base64 PNG for the MCP tool (truncated preview).
async fn screenshot_run(url: &str) -> anyhow::Result<String> {
    let png = screenshot_png(url).await?;
    use base64::{Engine as _, engine::general_purpose};
    let b64 = general_purpose::STANDARD.encode(&png);
    Ok(if b64.len() > 20000 { format!("{}...[truncated {} bytes]", &b64[..20000], png.len()) } else { b64 })
}

// ── CLI ─────────────────────────────────────────────────────────────────────

const USAGE: &str = "\
scour - local web scraper for agents (HTTP first, headless Chrome when blocked)

usage:
  scour <url> [focus...] [flags]          scrape one page (same as `scrape`)
  scour scrape <url> [focus...] [--max-chars N] [--offset N] [--json] [--stats]
  scour batch [urls...] [--focus Q] [--max-chars N] [--parallel N] [--json] [--stats]
                                          urls from stdin if none (# lines are comments)
  scour search <query...> [--max N] [--site D] [--freshness day|week|month|year]
               [--fetch N] [--focus Q] [--max-chars N] [--json] [--stats]
                                          --fetch N scrapes the top N hits (max 8)
  scour gh <owner/repo|github-url>...     GitHub repo: stars, license, latest release
  scour pkg <crates|npm|pypi> <name>...   latest version + description
  scour links <url> [--filter S] [--same-site] [--max N]
  scour crawl <url> [--max-pages N] [--depth N] [--focus Q] [--max-chars N] [--sitemap] [--json] [--stats]
  scour screenshot <url> [-o PATH]        PNG (default /tmp/scour-<unix_ts>.png)
  scour cache-clear
  scour --version | -V | --help | -h
  scour                                   MCP stdio server

defaults: --max-chars 6000 (scrape, batch), 2500 (search --fetch, crawl per page);
          --max-pages 10 (max 50); --depth 2; --max 10 (search), 100 (links)
";

/// `--help`, `-h` or `help` as the first token.
fn is_help(arg: Option<&str>) -> bool {
    matches!(arg, Some("--help" | "-h" | "help"))
}

/// Usage error: message and usage on stderr, exit 2.
fn usage(msg: &str) -> ! {
    eprintln!("error: {msg}\n\n{USAGE}");
    std::process::exit(2)
}

/// Markdown body plus the pagination hint when there is more to read.
fn with_hint(markdown: &str, next_offset: Option<usize>) -> String {
    match next_offset {
        Some(n) => format!("{markdown}\n\n<!-- truncated: rerun with --offset {n} for more -->"),
        None => markdown.to_string(),
    }
}

// Shared markdown emitter for the shim and the `scrape` subcommand.
fn emit_scrape(r: &ScrapeResult) {
    if let Some(n) = &r.note { eprintln!("warning: {n}"); }
    if let Some(a) = &r.next_action { eprintln!("next: {a}"); }
    eprintln!("page_type: {} quality: {:.2} links: {} headings: {} next_offset: {:?} media: {}", r.page_type, r.quality_score, r.links.len(), r.headings.len(), r.next_offset, r.media.len());
    println!("# {}\n<!-- tier: {} content_ok: {} chars: {} -->\n\n{}", r.title, r.tier, r.content_ok, r.total_chars, with_hint(&r.markdown, r.next_offset));
}

/// Value of the flag at `args[*i]`; leaves `*i` on the value.
fn flag_value<'a>(args: &'a [String], i: &mut usize) -> Result<&'a str, String> {
    *i += 1;
    match args.get(*i) {
        Some(v) => Ok(v.as_str()),
        None => Err(format!("{} needs a value", args[*i - 1])),
    }
}

fn parse_num<T: std::str::FromStr>(s: &str, flag: &str) -> Result<T, String> {
    s.parse().map_err(|_| format!("{flag} needs a number, got '{s}'"))
}

/// Non-flag args for subcommands without flags beyond the list itself.
fn positionals(args: &[String]) -> Result<Vec<String>, String> {
    if let Some(f) = args.iter().find(|a| a.starts_with('-')) {
        return Err(format!("unknown flag {f}"));
    }
    Ok(args.to_vec())
}

fn one_url(mut words: Vec<String>) -> Result<String, String> {
    match words.len() {
        0 => Err("missing <url>".into()),
        1 => Ok(words.remove(0)),
        _ => Err(format!("unexpected argument {}", words[1])),
    }
}

#[derive(Debug, PartialEq)]
struct ScrapeOpts {
    url: String,
    focus: Option<String>,
    max_chars: usize,
    offset: Option<usize>,
    json: bool,
    stats: bool,
}

fn parse_scrape(args: &[String]) -> Result<ScrapeOpts, String> {
    let mut o = ScrapeOpts { url: String::new(), focus: None, max_chars: 6000, offset: None, json: false, stats: false };
    let mut words = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--max-chars" => o.max_chars = parse_num(flag_value(args, &mut i)?, "--max-chars")?,
            "--offset" => o.offset = Some(parse_num(flag_value(args, &mut i)?, "--offset")?),
            "--json" => o.json = true,
            "--stats" => o.stats = true,
            s if s.starts_with('-') => return Err(format!("unknown flag {s}")),
            s => words.push(s.to_string()),
        }
        i += 1;
    }
    if words.is_empty() {
        return Err("missing <url>".into());
    }
    o.url = words.remove(0);
    o.focus = (!words.is_empty()).then(|| words.join(" "));
    Ok(o)
}

#[derive(Debug, PartialEq)]
struct BatchOpts {
    urls: Vec<String>,
    focus: Option<String>,
    max_chars: usize,
    parallel: usize,
    json: bool,
    stats: bool,
}

fn parse_batch(args: &[String]) -> Result<BatchOpts, String> {
    let mut o = BatchOpts { urls: Vec::new(), focus: None, max_chars: 6000, parallel: 4, json: false, stats: false };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--focus" => o.focus = Some(flag_value(args, &mut i)?.to_string()),
            "--max-chars" => o.max_chars = parse_num(flag_value(args, &mut i)?, "--max-chars")?,
            "--parallel" => o.parallel = parse_num(flag_value(args, &mut i)?, "--parallel")?,
            "--json" => o.json = true,
            "--stats" => o.stats = true,
            s if s.starts_with('-') => return Err(format!("unknown flag {s}")),
            s => o.urls.push(s.to_string()),
        }
        i += 1;
    }
    Ok(o)
}

/// Batch URLs from stdin: one per line; blanks and `#` comments are skipped.
fn parse_url_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

#[derive(Debug, PartialEq)]
struct SearchOpts {
    query: String,
    max: usize,
    site: Option<String>,
    freshness: Option<String>,
    fetch: usize,
    focus: String,
    max_chars: usize,
    json: bool,
    stats: bool,
}

fn parse_search(args: &[String]) -> Result<SearchOpts, String> {
    let mut o = SearchOpts {
        query: String::new(),
        max: 10,
        site: None,
        freshness: None,
        fetch: 0,
        focus: String::new(),
        max_chars: 2500,
        json: false,
        stats: false,
    };
    let mut words = Vec::new();
    let mut focus = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--max" => o.max = parse_num(flag_value(args, &mut i)?, "--max")?,
            "--site" => o.site = Some(flag_value(args, &mut i)?.to_string()),
            "--freshness" => o.freshness = Some(flag_value(args, &mut i)?.to_string()),
            "--fetch" => o.fetch = parse_num::<usize>(flag_value(args, &mut i)?, "--fetch")?.min(8),
            "--focus" => focus = Some(flag_value(args, &mut i)?.to_string()),
            "--max-chars" => o.max_chars = parse_num(flag_value(args, &mut i)?, "--max-chars")?,
            "--json" => o.json = true,
            "--stats" => o.stats = true,
            s if s.starts_with('-') => return Err(format!("unknown flag {s}")),
            s => words.push(s.to_string()),
        }
        i += 1;
    }
    if words.is_empty() {
        return Err("missing <query>".into());
    }
    o.query = words.join(" ");
    o.focus = focus.unwrap_or_else(|| o.query.clone());
    o.max = o.max.clamp(1, 20);
    Ok(o)
}

#[derive(Debug, PartialEq)]
struct LinksOpts {
    url: String,
    filter: Option<String>,
    same_site: bool,
    max: usize,
}

fn parse_links(args: &[String]) -> Result<LinksOpts, String> {
    let mut words = Vec::new();
    let mut filter = None;
    let mut same_site = false;
    let mut max = 100;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--filter" => filter = Some(flag_value(args, &mut i)?.to_string()),
            "--same-site" => same_site = true,
            "--max" => max = parse_num(flag_value(args, &mut i)?, "--max")?,
            s if s.starts_with('-') => return Err(format!("unknown flag {s}")),
            s => words.push(s.to_string()),
        }
        i += 1;
    }
    Ok(LinksOpts { url: one_url(words)?, filter, same_site, max })
}

#[derive(Debug, PartialEq)]
struct CrawlOpts {
    url: String,
    max_pages: usize,
    depth: usize,
    focus: Option<String>,
    max_chars: usize,
    sitemap: bool,
    json: bool,
    stats: bool,
}

fn parse_crawl(args: &[String]) -> Result<CrawlOpts, String> {
    let mut words = Vec::new();
    let mut o = CrawlOpts { url: String::new(), max_pages: 10, depth: 2, focus: None, max_chars: 2500, sitemap: false, json: false, stats: false };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--max-pages" => o.max_pages = parse_num(flag_value(args, &mut i)?, "--max-pages")?,
            "--depth" => o.depth = parse_num(flag_value(args, &mut i)?, "--depth")?,
            "--focus" => o.focus = Some(flag_value(args, &mut i)?.to_string()),
            "--max-chars" => o.max_chars = parse_num(flag_value(args, &mut i)?, "--max-chars")?,
            "--sitemap" => o.sitemap = true,
            "--json" => o.json = true,
            "--stats" => o.stats = true,
            s if s.starts_with('-') => return Err(format!("unknown flag {s}")),
            s => words.push(s.to_string()),
        }
        i += 1;
    }
    o.url = one_url(words)?;
    o.max_pages = o.max_pages.clamp(1, 50);
    Ok(o)
}

/// `(url, -o path)`.
fn parse_screenshot(args: &[String]) -> Result<(String, Option<String>), String> {
    let mut words = Vec::new();
    let mut out = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" => out = Some(flag_value(args, &mut i)?.to_string()),
            s if s.starts_with('-') => return Err(format!("unknown flag {s}")),
            s => words.push(s.to_string()),
        }
        i += 1;
    }
    Ok((one_url(words)?, out))
}

/// Every `<a href>` on the page as absolute URLs. Uncapped and unfiltered (unlike
/// `extractor::extract`, which keeps only the first 100 text-bearing links).
fn page_links(html: &str, base_url: &str) -> Vec<extractor::Link> {
    let doc = scraper::Html::parse_document(html);
    let sel = scraper::Selector::parse("a[href]").unwrap();
    let base = url::Url::parse(base_url).ok();
    doc.select(&sel)
        .filter_map(|el| {
            let href = el.value().attr("href")?.trim();
            if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
                return None;
            }
            let abs = match &base {
                Some(b) => b.join(href).ok()?.to_string(),
                None => href.to_string(),
            };
            Some(extractor::Link { href: abs, text: el.text().collect::<Vec<_>>().join(" ") })
        })
        .collect()
}

/// `links`: dedupe by href, optional case-insensitive substring on href or
/// text, optional same-host filter, capped at `max`. Returns (text, href).
fn filter_links(links: &[extractor::Link], page_url: &str, filter: Option<&str>, same_site: bool, max: usize) -> Vec<(String, String)> {
    let page_host = url::Url::parse(page_url).ok().and_then(|u| u.host_str().map(str::to_lowercase));
    let needle = filter.map(str::to_lowercase);
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for l in links {
        if out.len() >= max {
            break;
        }
        if seen.contains(&l.href) {
            continue;
        }
        let text = l.text.split_whitespace().collect::<Vec<_>>().join(" ");
        if let Some(n) = &needle
            && !l.href.to_lowercase().contains(n.as_str())
            && !text.to_lowercase().contains(n.as_str())
        {
            continue;
        }
        if same_site {
            let host = url::Url::parse(&l.href).ok().and_then(|u| u.host_str().map(str::to_lowercase));
            if host.is_none() || host != page_host {
                continue;
            }
        }
        seen.insert(l.href.clone());
        let text: String = if text.is_empty() { "(no text)".to_string() } else { text.chars().take(100).collect() };
        out.push((text, l.href.clone()));
    }
    out
}

/// `gh`/`pkg` lookups printed in input order, blank-line separated. Failures
/// print inline as `error: <key>: <msg>`. Returns false if every lookup failed.
fn print_lookups(keys: &[String], results: Vec<anyhow::Result<String>>) -> bool {
    let mut any_ok = false;
    for (i, (key, res)) in keys.iter().zip(results).enumerate() {
        if i > 0 {
            println!();
        }
        match res {
            Ok(md) => {
                any_ok = true;
                println!("{}", md.trim_end());
            }
            Err(e) => println!("error: {key}: {e}"),
        }
    }
    any_ok
}

/// One fetched URL: input URL plus its scrape (or error), in input order.
type PageFetch = (String, anyhow::Result<(ScrapeResult, Stats)>);

/// Scrapes `urls` concurrently (at most `parallel`, clamped 1..=8), results in input order.
async fn fetch_all(urls: Vec<String>, focus: Option<String>, max_chars: usize, parallel: usize) -> Vec<PageFetch> {
    let n = urls.len();
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(parallel.clamp(1, 8)));
    let mut set = tokio::task::JoinSet::new();
    for (idx, url) in urls.into_iter().enumerate() {
        let sem = sem.clone();
        let focus = focus.clone();
        set.spawn(async move {
            let _permit = sem.acquire_owned().await;
            let res = run_ext_stats(&url, focus.as_deref(), max_chars, true, None, false, None, None).await;
            (idx, url, res.map(|(r, mut st)| {
                st.op = "batch_item";
                (r, st)
            }))
        });
    }
    let mut slots: Vec<Option<PageFetch>> = (0..n).map(|_| None).collect();
    while let Some(joined) = set.join_next().await {
        if let Ok((idx, url, res)) = joined {
            slots[idx] = Some((url, res));
        }
    }
    slots.into_iter().flatten().collect()
}

/// Markdown section for one scraped page (`## url`, meta comment, body).
fn fetched_md(url: &str, r: &ScrapeResult) -> String {
    format!("## {url}\n<!-- tier: {} content_ok: {} chars: {} -->\n\n{}", r.tier, r.content_ok, r.total_chars, with_hint(&r.markdown, r.next_offset))
}

fn print_pages_md(pages: &[PageFetch]) {
    for (url, res) in pages {
        match res {
            Ok((r, _)) => println!("{}\n", fetched_md(url, r)),
            Err(e) => println!("## {url}\n<!-- tier: error content_ok: false chars: 0 -->\n\n_fetch failed: {e}_\n"),
        }
    }
}

/// JSON for one page; failed fetches become `{url, content_ok:false, error}`.
fn page_json(url: &str, res: &anyhow::Result<(ScrapeResult, Stats)>) -> serde_json::Value {
    match res {
        Ok((r, _)) => serde_json::to_value(r).unwrap_or_default(),
        Err(e) => serde_json::json!({"url": url, "content_ok": false, "error": e.to_string()}),
    }
}

fn print_item_stats(pages: &[PageFetch]) {
    for (_, res) in pages {
        if let Ok((_, st)) = res {
            st.print();
        }
    }
}

async fn cmd_scrape(args: &[String]) -> anyhow::Result<()> {
    let o = parse_scrape(args).unwrap_or_else(|e| usage(&e));
    let (r, st) = run_ext_stats(&o.url, o.focus.as_deref(), o.max_chars, true, o.offset, false, None, None).await?;
    if o.json {
        println!("{}", serde_json::to_string(&r)?);
    } else {
        emit_scrape(&r);
    }
    if o.stats {
        st.print();
    }
    Ok(())
}

async fn cmd_search(args: &[String]) -> anyhow::Result<()> {
    let o = parse_search(args).unwrap_or_else(|e| usage(&e));
    let t0 = std::time::Instant::now();
    let hits = search_run(&o.query, o.max, o.site.as_deref(), None, o.freshness.as_deref()).await?;
    let ms = t0.elapsed().as_millis();
    let pages = if o.fetch > 0 {
        let urls: Vec<String> = hits.iter().take(o.fetch).map(|h| h.url.clone()).collect();
        fetch_all(urls, Some(o.focus.clone()), o.max_chars, 4).await
    } else {
        Vec::new()
    };
    let sr = SearchResult { query: o.query.clone(), total: hits.len(), hits };
    if o.json {
        let mut v = serde_json::to_value(&sr)?;
        if o.fetch > 0 {
            v["pages"] = pages.iter().map(|(u, r)| page_json(u, r)).collect();
        }
        println!("{v}");
    } else {
        println!("# {}", sr.query);
        for (idx, h) in sr.hits.iter().enumerate() {
            println!("{}. {} — {}", idx + 1, h.title, h.url);
            if !h.snippet.is_empty() {
                println!("   {}", h.snippet);
            }
        }
        if o.fetch > 0 {
            println!();
            print_pages_md(&pages);
        }
    }
    if o.stats {
        print_item_stats(&pages);
        eprintln!("stats: {}", serde_json::json!({"op":"search","query":&o.query,"hits":sr.total,"ms":ms}));
    }
    Ok(())
}

async fn cmd_batch(args: &[String]) -> anyhow::Result<()> {
    let mut o = parse_batch(args).unwrap_or_else(|e| usage(&e));
    if o.urls.is_empty() {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        o.urls = parse_url_lines(&buf);
    }
    o.urls.truncate(20);
    if o.urls.is_empty() {
        usage("no URLs given (args or stdin)");
    }
    let n = o.urls.len();
    let t0 = std::time::Instant::now();
    let pages = fetch_all(o.urls, o.focus.clone(), o.max_chars, o.parallel).await;
    if o.json {
        let arr: Vec<serde_json::Value> = pages.iter().map(|(u, r)| page_json(u, r)).collect();
        println!("{}", serde_json::Value::Array(arr));
    } else {
        print_pages_md(&pages);
    }
    if o.stats {
        print_item_stats(&pages);
        let ok = pages.iter().filter(|(_, r)| matches!(r, Ok((d, _)) if d.content_ok)).count();
        let chars: usize = pages
            .iter()
            .filter_map(|(_, r)| r.as_ref().ok())
            .map(|(d, _)| d.markdown.chars().count())
            .sum();
        eprintln!("stats: {}", serde_json::json!({"op":"batch","n":n,"ms":t0.elapsed().as_millis(),"chars_out":chars,"est_tokens":chars/4,"ok":ok}));
    }
    Ok(())
}

async fn cmd_gh(args: &[String]) -> anyhow::Result<()> {
    let specs = positionals(args).unwrap_or_else(|e| usage(&e));
    if specs.is_empty() {
        usage("gh needs at least one owner/repo or github URL");
    }
    let res = futures::future::join_all(specs.iter().map(|s| tools::gh(s))).await;
    if !print_lookups(&specs, res) {
        std::process::exit(1);
    }
    Ok(())
}

async fn cmd_pkg(args: &[String]) -> anyhow::Result<()> {
    let mut words = positionals(args).unwrap_or_else(|e| usage(&e));
    if words.len() < 2 {
        usage("pkg needs a registry (crates|npm|pypi) and at least one name");
    }
    let registry = words.remove(0);
    let res = futures::future::join_all(words.iter().map(|n| tools::pkg(&registry, n))).await;
    if !print_lookups(&words, res) {
        std::process::exit(1);
    }
    Ok(())
}

async fn cmd_links(args: &[String]) -> anyhow::Result<()> {
    let o = parse_links(args).unwrap_or_else(|e| usage(&e));
    validate(&o.url)?;
    let got = fetcher::fetch(&o.url).await?;
    for (text, href) in filter_links(&page_links(&got.html, &o.url), &o.url, o.filter.as_deref(), o.same_site, o.max) {
        println!("{text} — {href}");
    }
    Ok(())
}

async fn cmd_crawl(args: &[String]) -> anyhow::Result<()> {
    let o = parse_crawl(args).unwrap_or_else(|e| usage(&e));
    let t0 = std::time::Instant::now();
    let res = crawl_run(&o.url, o.max_pages, o.depth, o.focus.as_deref(), o.max_chars, Some(o.sitemap), None, None).await?;
    if o.json {
        println!("{}", serde_json::to_string(&res)?);
    } else {
        for p in &res.pages {
            println!("{}\n", fetched_md(&p.url, p));
        }
    }
    if o.stats {
        eprintln!("stats: {}", serde_json::json!({"op":"crawl","start_url":&o.url,"pages":res.pages.len(),"visited":res.visited.len(),"total_chars":res.total_chars,"ms":t0.elapsed().as_millis()}));
    }
    Ok(())
}

async fn cmd_screenshot(args: &[String]) -> anyhow::Result<()> {
    let (url, out) = parse_screenshot(args).unwrap_or_else(|e| usage(&e));
    let png = screenshot_png(&url).await?;
    let path = out.unwrap_or_else(|| {
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        format!("/tmp/scour-{ts}.png")
    });
    std::fs::write(&path, &png)?;
    println!("{path}");
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str);
    if is_help(cmd) {
        print!("{USAGE}");
        return Ok(());
    }
    match cmd {
        None => {}
        Some("--version" | "-V") => {
            println!("{}", serde_json::to_string(&version_info()).unwrap_or_default());
            return Ok(());
        }
        Some("scrape") => return cmd_scrape(&args[1..]).await,
        Some("search") => return cmd_search(&args[1..]).await,
        Some("batch") => return cmd_batch(&args[1..]).await,
        Some("gh") => return cmd_gh(&args[1..]).await,
        Some("pkg") => return cmd_pkg(&args[1..]).await,
        Some("links") => return cmd_links(&args[1..]).await,
        Some("crawl") => return cmd_crawl(&args[1..]).await,
        Some("screenshot") => return cmd_screenshot(&args[1..]).await,
        Some("cache-clear") => {
            cache::cache_clear();
            eprintln!("cache cleared");
            return Ok(());
        }
        Some(s) if s.starts_with('-') => usage(&format!("unknown option {s}")),
        // Bare URL: shorthand for `scrape <url> [focus...] [flags]`.
        Some(_) => return cmd_scrape(&args).await,
    }
    use rmcp::ServiceExt;
    let service = Scour::new().serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn help_flags_detected() {
        for h in ["--help", "-h", "help"] {
            assert!(is_help(Some(h)));
        }
        assert!(!is_help(None));
        assert!(!is_help(Some("https://example.com")));
        assert!(!is_help(Some("scrape")));
    }

    #[test]
    fn search_fetch_defaults() {
        let o = parse_search(&s(&["axum middleware", "--fetch", "2"])).unwrap();
        assert_eq!((o.fetch, o.max, o.max_chars), (2, 10, 2500));
        assert_eq!(o.focus, "axum middleware");
        assert_eq!(parse_search(&s(&["x", "--fetch", "50"])).unwrap().fetch, 8);
        let o = parse_search(&s(&["x", "--focus", "routing", "--max-chars", "300"])).unwrap();
        assert_eq!((o.focus.as_str(), o.max_chars), ("routing", 300));
        assert!(parse_search(&s(&["x", "--bogus"])).is_err());
        assert!(parse_search(&s(&["--fetch", "2"])).is_err());
    }

    #[test]
    fn links_filter_dedupes_and_scopes() {
        let l = |href: &str, text: &str| extractor::Link { href: href.into(), text: text.into() };
        let links = vec![
            l("https://docs.rs/axum/latest/axum/middleware/index.html", "Middleware"),
            l("https://docs.rs/axum/latest/axum/middleware/index.html", "Middleware"),
            l("https://github.com/tokio-rs/axum", "GitHub"),
            l("https://docs.rs/axum/latest/axum/routing/", "  Routing\n  Guide "),
        ];
        let page = "https://docs.rs/axum/latest/axum/";
        let all = filter_links(&links, page, None, false, 100);
        assert_eq!(all.len(), 3);
        assert_eq!(all[2].0, "Routing Guide");
        assert_eq!(filter_links(&links, page, Some("MIDDLEWARE"), false, 100).len(), 1);
        assert_eq!(filter_links(&links, page, Some("github"), false, 100).len(), 1);
        let same = filter_links(&links, page, None, true, 100);
        assert_eq!(same.len(), 2);
        assert!(same.iter().all(|(_, h)| h.starts_with("https://docs.rs")));
        assert_eq!(filter_links(&links, page, None, false, 1).len(), 1);
    }

    #[test]
    fn page_links_resolves_and_skips_anchors() {
        let html = r##"<a href="/x">X</a><a href="#top">top</a><a href="javascript:void(0)">j</a><a href="https://o.com/y"></a>"##;
        let hrefs: Vec<String> = page_links(html, "https://e.com/a/").into_iter().map(|l| l.href).collect();
        assert_eq!(hrefs, vec!["https://e.com/x", "https://o.com/y"]);
    }

    #[test]
    fn truncation_hint_only_when_more_to_read() {
        assert_eq!(with_hint("abc", None), "abc");
        assert_eq!(with_hint("abc", Some(150)), "abc\n\n<!-- truncated: rerun with --offset 150 for more -->");
    }

    #[test]
    fn truncate_never_splits_a_char() {
        let (out, cut) = truncate(&"é".repeat(10), 5);
        assert!(cut);
        assert_eq!(out, "éé");
    }

    #[test]
    fn scrape_parses_offset_focus_and_rejects_unknown() {
        let o = parse_scrape(&s(&["https://e.com", "rust", "traits", "--offset", "6000", "--json"])).unwrap();
        assert_eq!(o.url, "https://e.com");
        assert_eq!(o.focus.as_deref(), Some("rust traits"));
        assert_eq!((o.offset, o.max_chars, o.json), (Some(6000), 6000, true));
        assert!(parse_scrape(&s(&["https://e.com", "--nope"])).is_err());
        assert!(parse_scrape(&s(&["--stats"])).is_err());
    }

    #[test]
    fn stdin_urls_trimmed_and_comments_skipped() {
        let got = parse_url_lines("  https://a.com \n\n# note\nhttps://b.com\n   # indented\n");
        assert_eq!(got, vec!["https://a.com", "https://b.com"]);
    }

    #[test]
    fn crawl_caps_max_pages() {
        assert_eq!(parse_crawl(&s(&["https://e.com", "--max-pages", "999"])).unwrap().max_pages, 50);
        assert_eq!(parse_crawl(&s(&["https://e.com"])).unwrap().max_pages, 10);
    }
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
        let out = super::focus_filter(md, "cats", 40);
        assert!(out.contains("cats and yarn") && out.contains("more cats"));
        assert!(!out.contains("unrelated filler"));
        assert!(out.find("yarn").unwrap() < out.find("sleeping").unwrap());
        assert_eq!(super::focus_filter(md, "zebra", 40), md);
    }

    #[test]
    fn focus_bm25_heading_boost() {
        let md = "# Cats overview\n\nabout dogs\n\n## Cats sleeping habits\n\nmore dogs";
        let out = super::focus_filter(md, "cats", 2);
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
        assert_eq!(super::normalize_url("https://example.com/a#frag"), "https://example.com/a");
        assert_eq!(super::normalize_url("https://example.com/b/"), "https://example.com/b");
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
        let next = if truncated { Some(offset + out.len()) } else { None };
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

    #[test]
    fn scrape_request_deserializes_backwards_compat() {
        let json = r#"{"url":"https://example.com"}"#;
        let req: super::ScrapeRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.url, "https://example.com");
        assert!(req.offset.is_none());
        assert!(req.include_media.is_none());
        assert!(req.actions.is_none());
        assert!(req.pages.is_none());
    }

    #[test]
    fn action_deser_click_and_fill() {
        let j1 = r##"{"click":"#button"}"##;
        let a1: super::Action = serde_json::from_str(j1).unwrap();
        match a1 { super::Action::Click(s) => assert_eq!(s, "#button"), _ => panic!("wrong") }
        let j2 = r##"{"fill":{"selector":"#input","text":"hello"}}"##;
        let a2: super::Action = serde_json::from_str(j2).unwrap();
        match a2 { super::Action::Fill { selector, text } => { assert_eq!(selector, "#input"); assert_eq!(text, "hello"); }, _ => panic!("wrong") }
    }
}
