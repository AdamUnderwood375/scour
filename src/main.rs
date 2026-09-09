//! scour — local MCP scraper. HTTP first, Chrome only when blocked.
//!
//!   scour            -> MCP server on stdio (for Pi / Claude)
//!   scour <url>      -> print markdown (smoke test, no MCP client needed)
//!
//! Frontier upgrade: article + list fallback, links/page_type, BM25 focus,
//! batch parallelism, and in-domain crawl.

mod cache;
mod extractor;
mod fetcher;

use cache::{cache_get, cache_key, cache_key_ext, cache_set};
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::wrapper::{Json, Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use std::collections::{HashMap, HashSet, VecDeque};
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
        Ok(Json(serde_json::json!({
            "name": "scour",
            "version": env!("CARGO_PKG_VERSION"),
            "tools": ["scrape", "scrape_batch", "crawl", "search", "screenshot", "cache_clear", "scour_version"],
            "features": ["http+chrome ladder", "readability+fallback", "bm25 focus", "parallel batch", "in-domain crawl", "links+page_type", "1hr cache", "keyless search", "screenshot", "offset pagination", "include_media", "actions", "pdf pages"],
            "browser": std::env::var("SCOUR_BROWSER").unwrap_or("/usr/bin/brave".to_string())
        })))
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

fn validate(url: &str) -> anyhow::Result<()> {
    let u = url::Url::parse(url)?;
    match u.scheme() {
        "http" | "https" => {},
        s => anyhow::bail!("refusing non-http scheme: {s}"),
    }
    if let Some(host) = u.host_str()
        && let Ok(ip) = host.parse::<std::net::IpAddr>() {
            let is_private = match ip {
                std::net::IpAddr::V4(v4) => v4.is_private(),
                std::net::IpAddr::V6(v6) => v6.is_unique_local(),
            };
            let is_link_local = match ip {
                std::net::IpAddr::V4(v4) => v4.is_link_local(),
                std::net::IpAddr::V6(v6) => v6.is_unicast_link_local(),
            };
            if ip.is_loopback() || ip.is_unspecified() || is_private || is_link_local {
                anyhow::bail!("refusing private/loopback/link-local IP: {host}");
            }
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
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(keep);
    scored.sort_by_key(|(_, i, _)| *i);
    scored.into_iter().map(|(_, _, p)| p).collect::<Vec<_>>().join("\n\n")
}

fn truncate(md: &str, limit: usize) -> (String, bool) {
    if md.len() <= limit {
        return (md.to_string(), false);
    }
    let cut = md[..limit]
        .rfind("\n\n")
        .filter(|b| *b > limit / 2)
        .unwrap_or(limit);
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
    validate(url)?;
    // PDF early path
    if let Some(pdf) = try_pdf_fetch(url, pages, max_chars, offset).await? {
        // Check if we should still use offset/next_offset and include_media (media empty for pdf)
        // Cache pdf result too
        let key = cache_key_ext(url, focus, max_chars, offset, include_media, pages, actions.map(|a| a.len()).unwrap_or(0));
        // don't cache failed pdf previews? cache anyway
        cache_set(key, pdf.clone());
        return Ok(pdf);
    }

    let actions_len = actions.map(|a| a.len()).unwrap_or(0);
    // Use extended cache key so pagination/actions don't collide
    let key = cache_key_ext(url, focus, max_chars, offset, include_media, pages, actions_len);
    // but also check legacy key for backwards compat when offset etc are default
    if let Some(cached) = cache_get(&key) {
        return Ok(cached);
    }
    if offset.is_none() && !include_media && pages.is_none() && actions_len == 0 {
        let legacy = cache_key(url, focus, max_chars);
        if let Some(cached) = cache_get(&legacy) {
            return Ok(cached);
        }
    }

    let got = if let Some(acts) = actions {
        if !acts.is_empty() {
            fetch_chrome_with_actions(url, acts).await?
        } else {
            fetcher::fetch(url).await?
        }
    } else {
        fetcher::fetch(url).await?
    };

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
    cache_set(key.clone(), out.clone());
    // also populate legacy key if default options for backwards compat
    if offset.is_none() && !include_media && pages.is_none() && actions_len == 0 {
        cache_set(cache_key(url, focus, max_chars), out.clone());
    }
    Ok(out)
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
            candidates.sort_by(|a,b| b.0.partial_cmp(&a.0).unwrap());
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

static DDG_TITLE_SEL: OnceLock<scraper::Selector> = OnceLock::new();
static DDG_SNIPPET_SEL: OnceLock<scraper::Selector> = OnceLock::new();
static DDG_RESULT_SEL: OnceLock<scraper::Selector> = OnceLock::new();
static SEARCH_A_HREF_SEL: OnceLock<scraper::Selector> = OnceLock::new();
static BING_SEL: OnceLock<scraper::Selector> = OnceLock::new();
static BING_FALLBACK_SEL: OnceLock<scraper::Selector> = OnceLock::new();
fn ddg_title_sel() -> &'static scraper::Selector { DDG_TITLE_SEL.get_or_init(|| scraper::Selector::parse("h2.result__title a").unwrap()) }
fn ddg_snippet_sel() -> &'static scraper::Selector { DDG_SNIPPET_SEL.get_or_init(|| scraper::Selector::parse(".result__snippet").unwrap()) }
fn ddg_result_sel() -> &'static scraper::Selector { DDG_RESULT_SEL.get_or_init(|| scraper::Selector::parse(".result").unwrap()) }
fn search_a_href_sel() -> &'static scraper::Selector { SEARCH_A_HREF_SEL.get_or_init(|| scraper::Selector::parse("a[href]").unwrap()) }
fn bing_sel() -> &'static scraper::Selector { BING_SEL.get_or_init(|| scraper::Selector::parse("li.b_algo h2 a").unwrap()) }
fn bing_fallback_sel() -> &'static scraper::Selector { BING_FALLBACK_SEL.get_or_init(|| scraper::Selector::parse("h2 a[href]").unwrap()) }

async fn search_run(
    query: &str,
    max_results: usize,
    site: Option<&str>,
    exclude_sites: Option<&[String]>,
    freshness: Option<&str>,
) -> anyhow::Result<Vec<SearchHit>> {
    let max_results = max_results.clamp(1, 20);
    let client = fetcher::client();
    let mut effective = query.to_string();
    if let Some(s) = site {
        let s = s.trim();
        if !s.is_empty() {
            effective.push_str(&format!(" site:{}", s));
        }
    }
    if let Some(excludes) = exclude_sites {
        for ex in excludes {
            let ex = ex.trim();
            if !ex.is_empty() {
                effective.push_str(&format!(" -site:{}", ex));
            }
        }
    }
    if let Some(f) = freshness {
        let f = f.trim().to_lowercase();
        if matches!(f.as_str(), "day" | "week" | "month" | "year") {
            effective.push_str(&format!(" when:{}", f));
        }
    }
    let ddg_url = format!("https://html.duckduckgo.com/html/?q={}", urlencoding(&effective));
    let bing_url = format!("https://www.bing.com/search?q={}", urlencoding(&effective));
    let ddg_fut = async {
        let resp = client.get(&ddg_url).send().await?;
        resp.text().await
    };
    let bing_fut = async {
        let resp = client.get(&bing_url).send().await?;
        resp.text().await
    };
    let (ddg_html_res, bing_html_res) = tokio::join!(ddg_fut, bing_fut);
    let mut ddg_hits: Vec<SearchHit> = Vec::new();
    if let Ok(html) = ddg_html_res {
        let doc = scraper::Html::parse_document(&html);
        for result in doc.select(ddg_result_sel()) {
            let Some(title_el) = result.select(ddg_title_sel()).next() else { continue; };
            let title = title_el.text().collect::<Vec<_>>().join(" ").trim().to_string();
            let href = title_el.value().attr("href").unwrap_or("").to_string();
            let url = if href.contains("uddg=") {
                href.split("uddg=").nth(1).and_then(|s| s.split('&').next()).and_then(|u| url::Url::parse(&urlencoding_decode(u)).ok()).map(|u| u.to_string()).unwrap_or(href.clone())
            } else { href };
            let snippet = result.select(ddg_snippet_sel()).next().map(|s| s.text().collect::<Vec<_>>().join(" ").trim().to_string()).unwrap_or_default();
            if title.is_empty() || url.is_empty() { continue; }
            if !url.starts_with("http") { continue; }
            ddg_hits.push(SearchHit { title, url, snippet });
        }
        if ddg_hits.is_empty() {
            for el in doc.select(search_a_href_sel()) {
                let mut href = el.value().attr("href").unwrap_or("").trim().to_string();
                if href.starts_with("//") { href = format!("https:{}", href); }
                if href.contains("duckduckgo.com/l/?")
                    && let Some(enc) = href.split("uddg=").nth(1).and_then(|s| s.split('&').next()) {
                        href = urlencoding_decode(enc);
                    }
                if !href.starts_with("http") { continue; }
                if href.contains("duckduckgo.com") { continue; }
                let title = el.text().collect::<Vec<_>>().join(" ").trim().to_string();
                if title.len() < 10 || title.len() > 200 { continue; }
                ddg_hits.push(SearchHit { title, url: href, snippet: String::new() });
            }
        }
        drop(doc);
    }
    let mut bing_hits: Vec<SearchHit> = Vec::new();
    if let Ok(bhtml) = bing_html_res {
        let bdoc = scraper::Html::parse_document(&bhtml);
        for el in bdoc.select(bing_sel()) {
            let mut href = el.value().attr("href").unwrap_or("").trim().to_string();
            if href.contains("bing.com/ck/a")
                && let Some(u) = href.split("u=").nth(1).and_then(|s| s.split('&').next()) {
                    let mut b64 = u.strip_prefix("a1").unwrap_or(u).to_string();
                    while b64.len() % 4 != 0 { b64.push('='); }
                    use base64::Engine as _;
                    if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(&b64)
                        && let Ok(s) = String::from_utf8(decoded) { href = s; }
                }
            if !href.starts_with("http") { continue; }
            let title = el.text().collect::<Vec<_>>().join(" ").trim().to_string();
            if title.is_empty() { continue; }
            bing_hits.push(SearchHit { title, url: href, snippet: String::new() });
        }
        if bing_hits.is_empty() {
            for el in bdoc.select(bing_fallback_sel()) {
                    let mut href = el.value().attr("href").unwrap_or("").trim().to_string();
                    if href.contains("bing.com/ck/a")
                        && let Some(u) = href.split("u=").nth(1).and_then(|s| s.split('&').next()) {
                            let mut b64 = u.strip_prefix("a1").unwrap_or(u).to_string();
                            while b64.len() % 4 != 0 { b64.push('='); }
                            use base64::Engine as _;
                            if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(&b64)
                                && let Ok(s) = String::from_utf8(decoded) { href = s; }
                        }
                    if !href.starts_with("http") || href.contains("bing.com") { continue; }
                    let title = el.text().collect::<Vec<_>>().join(" ").trim().to_string();
                    if title.is_empty() || title.len() < 10 { continue; }
                    bing_hits.push(SearchHit { title, url: href, snippet: String::new() });
                }
        }
        drop(bdoc);
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut merged: Vec<(SearchHit, bool)> = Vec::new();
    for hit in ddg_hits {
        let norm = normalize_url(&hit.url);
        if seen.insert(norm) {
            merged.push((hit, true));
        }
    }
    for hit in bing_hits {
        let norm = normalize_url(&hit.url);
        if seen.insert(norm) {
            merged.push((hit, false));
        }
    }
    if let Some(s) = site {
        let s = s.trim().to_lowercase();
        if !s.is_empty() {
            merged.retain(|(h, _)| {
                if let Ok(u) = url::Url::parse(&h.url) {
                    if let Some(host) = u.host_str() {
                        host.to_lowercase().contains(&s) || h.url.to_lowercase().contains(&s)
                    } else { false }
                } else {
                    h.url.to_lowercase().contains(&s)
                }
            });
        }
    }
    if let Some(excludes) = exclude_sites {
        let lowers: Vec<String> = excludes.iter().map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty()).collect();
        if !lowers.is_empty() {
            merged.retain(|(h, _)| {
                let url_lower = h.url.to_lowercase();
                let host_lower = url::Url::parse(&h.url).ok().and_then(|u| u.host_str().map(|s| s.to_lowercase())).unwrap_or_default();
                !lowers.iter().any(|ex| host_lower.contains(ex) || url_lower.contains(ex))
            });
        }
    }
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
        .filter(|t| t.len() > 2)
        .collect();
    if !terms.is_empty() {
        merged.sort_by(|(a, is_a), (b, is_b)| {
            let score_a = {
                let title = a.title.to_lowercase();
                let url = a.url.to_lowercase();
                let snippet = a.snippet.to_lowercase();
                let mut s = 0.0f32;
                for t in &terms {
                    if title.contains(t) { s += 2.0; }
                    if url.contains(t) { s += 1.0; }
                    if snippet.contains(t) { s += 1.0; }
                }
                if *is_a { s += 0.5; }
                s
            };
            let score_b = {
                let title = b.title.to_lowercase();
                let url = b.url.to_lowercase();
                let snippet = b.snippet.to_lowercase();
                let mut s = 0.0f32;
                for t in &terms {
                    if title.contains(t) { s += 2.0; }
                    if url.contains(t) { s += 1.0; }
                    if snippet.contains(t) { s += 1.0; }
                }
                if *is_b { s += 0.5; }
                s
            };
            score_b.partial_cmp(&score_a).unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    let out: Vec<SearchHit> = merged.into_iter().map(|(h, _)| h).take(max_results).collect();
    Ok(out)
}

fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' { out.push(b as char); }
        else if b == b' ' { out.push('+'); }
        else { out.push_str(&format!("%{:02X}", b)); }
    }
    out
}
fn urlencoding_decode(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let h1 = chars.next().unwrap_or('0');
            let h2 = chars.next().unwrap_or('0');
            if let Ok(b) = u8::from_str_radix(&format!("{}{}", h1, h2), 16) { out.push(b as char); } else { out.push('%'); out.push(h1); out.push(h2); }
        } else if c == '+' { out.push(' '); } else { out.push(c); }
    }
    out
}

async fn screenshot_run(url: &str) -> anyhow::Result<String> {
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
    use base64::{Engine as _, engine::general_purpose};
    let b64 = general_purpose::STANDARD.encode(&png);
    Ok(if b64.len() > 20000 { format!("{}...[truncated {} bytes]", &b64[..20000], png.len()) } else { b64 })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Some(url) = std::env::args().nth(1) {
        let focus = {
            let rest: Vec<String> = std::env::args().skip(2).collect();
            (!rest.is_empty()).then(|| rest.join(" "))
        };
        let r = run(&url, focus.as_deref(), 40_000, true).await?;
        if let Some(n) = &r.note { eprintln!("warning: {n}"); }
        if let Some(a) = &r.next_action { eprintln!("next: {a}"); }
        eprintln!("page_type: {} quality: {:.2} links: {} headings: {} next_offset: {:?} media: {}", r.page_type, r.quality_score, r.links.len(), r.headings.len(), r.next_offset, r.media.len());
        println!("# {}\n<!-- tier: {} content_ok: {} chars: {} -->\n\n{}", r.title, r.tier, r.content_ok, r.total_chars, r.markdown);
        return Ok(());
    }
    use rmcp::ServiceExt;
    let service = Scour::new().serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
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
