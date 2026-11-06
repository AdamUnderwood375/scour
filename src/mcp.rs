//! The `Scour` rmcp server: request/response structs, #[tool] impl,
//! ServerHandler impl, and version_info.

use crate::cli::{failed_doc, gh_summary};
use crate::scrape::{crawl_run, run, run_ext, screenshot_run};
use crate::search::search_run;
use crate::types::*;
use crate::{cache, fetcher, tools};
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::wrapper::{Json, Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use std::collections::HashMap;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema, Default, Clone)]
pub struct ScrapeOptions {
    #[schemars(description = "Include image URLs (extract <img src> via scraper)")]
    #[serde(default)]
    pub include_media: Option<bool>,
    #[schemars(
        description = "Browser actions to perform before extraction (escalates to chrome: click, fill, press, wait, scroll)"
    )]
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
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct BatchResult {
    pub results: Vec<ScrapeResult>,
    pub total: usize,
    pub ok_count: usize,
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
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GithubRepoRequest {
    #[schemars(description = "owner/repo or any github.com URL")]
    pub repo: String,
    #[schemars(description = "Include a README excerpt (default false)")]
    #[serde(default)]
    pub readme: Option<bool>,
    #[schemars(description = "README excerpt cap in chars (default 1500)")]
    #[serde(default)]
    pub max_chars: Option<usize>,
    #[schemars(
        description = "Return only the README paragraphs relevant to this query, best-first"
    )]
    #[serde(default)]
    pub focus: Option<String>,
}
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct PackageInfoRequest {
    #[schemars(description = "Registry: crates | npm | pypi (aliases ok)")]
    pub registry: String,
    #[schemars(description = "Package name")]
    pub name: String,
}
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct TextResult {
    pub text: String,
}

#[derive(Clone)]
pub(crate) struct Scour {
    #[allow(dead_code)]
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

#[tool_router]
impl Scour {
    pub(crate) fn new() -> Self {
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
        .map_err(|e| {
            ErrorData::internal_error(format!("scrape failed for {}: {e}", req.url), None)
        })?;
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
            return Err(ErrorData::invalid_params(
                "urls must be 1..10".to_string(),
                None,
            ));
        }
        let focus = req.focus.clone();
        let max_chars = req.max_chars.unwrap_or(40_000);
        let handles: Vec<_> = req
            .urls
            .iter()
            .cloned()
            .map(|url| {
                let f = focus.clone();
                tokio::spawn(async move { run(&url, f.as_deref(), max_chars, true).await })
            })
            .collect();
        let mut ok = 0;
        let mut out = Vec::with_capacity(handles.len());
        for (url, handle) in req.urls.into_iter().zip(handles) {
            // A panicked task still yields a result for its URL.
            let res = handle.await.unwrap_or_else(|e| Err(anyhow::anyhow!("{e}")));
            let doc = match res {
                Ok(doc) => doc,
                Err(e) => failed_doc(&url, format!("fetch failed: {e}")),
            };
            if doc.content_ok {
                ok += 1;
            }
            out.push(doc);
        }
        Ok(Json(BatchResult {
            total: out.len(),
            ok_count: ok,
            results: out,
        }))
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
        let res = crawl_run(
            &req.url,
            max_pages,
            max_depth,
            req.focus.as_deref(),
            max_chars,
            req.sitemap,
            req.discover_only,
            req.max_total_chars,
        )
        .await
        .map_err(|e| {
            ErrorData::internal_error(format!("crawl failed for {}: {e}", req.url), None)
        })?;
        Ok(Json(res))
    }

    #[tool(description = "Clear the in-memory fetch cache (1hr TTL). Use to force a fresh fetch.")]
    async fn cache_clear(&self) -> Result<Json<serde_json::Value>, ErrorData> {
        cache::cache_clear();
        Ok(Json(serde_json::json!({"cleared": true})))
    }

    #[tool(
        description = "Look up a GitHub repo via the GitHub API (no HTML scraping; honors GITHUB_TOKEN for rate limits). Returns markdown: stars, language, latest release, description, and an optional focus-filtered README excerpt."
    )]
    async fn github_repo(
        &self,
        Parameters(req): Parameters<GithubRepoRequest>,
    ) -> Result<Json<TextResult>, ErrorData> {
        let text = gh_summary(
            &req.repo,
            req.readme.unwrap_or(false),
            req.focus.as_deref(),
            req.max_chars.unwrap_or(1500),
        )
        .await
        .map_err(|e| {
            ErrorData::internal_error(format!("github_repo failed for {}: {e}", req.repo), None)
        })?;
        Ok(Json(TextResult { text }))
    }

    #[tool(
        description = "Look up the latest package version via the registry APIs (crates.io / npm / PyPI; no HTML scraping, honors GITHUB_TOKEN where relevant). Returns markdown. `registry` accepts crates|npm|pypi and aliases."
    )]
    async fn package_info(
        &self,
        Parameters(req): Parameters<PackageInfoRequest>,
    ) -> Result<Json<TextResult>, ErrorData> {
        let text = tools::pkg(&req.registry, &req.name).await.map_err(|e| {
            ErrorData::internal_error(format!("package_info failed for {}: {e}", req.name), None)
        })?;
        Ok(Json(TextResult { text }))
    }

    #[tool(
        description = "Search the web (keyless, fused across Bing, Yahoo, Brave, DuckDuckGo and Mojeek; paced with per-engine cooldowns, deduped and ranked). Returns title/url/snippet hits. Use before `scrape` to discover URLs. Supports site:, freshness, and exclude_sites filters."
    )]
    async fn search(
        &self,
        Parameters(req): Parameters<SearchRequest>,
    ) -> Result<Json<SearchResult>, ErrorData> {
        let max = req.max_results.unwrap_or(10).clamp(1, 20);
        let (hits, engines, warning) = search_run(
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
            engines,
            warning,
        }))
    }

    #[tool(
        description = "Screenshot a page via headless Chromium (Brave/Chrome/Chromium or Playwright's Chromium; SCOUR_BROWSER to override). Returns base64 PNG (truncated preview)."
    )]
    async fn screenshot(
        &self,
        Parameters(req): Parameters<ScrapeRequest>,
    ) -> Result<Json<serde_json::Value>, ErrorData> {
        let png = screenshot_run(&req.url).await.map_err(|e| {
            ErrorData::internal_error(format!("screenshot failed for {}: {e}", req.url), None)
        })?;
        Ok(Json(
            serde_json::json!({"url": req.url, "png_base64": png, "note": "truncated base64, decode to view"}),
        ))
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

pub(crate) fn version_info() -> serde_json::Value {
    serde_json::json!({
        "name": "scour",
        "version": env!("CARGO_PKG_VERSION"),
        "tools": ["scrape", "scrape_batch", "crawl", "search", "github_repo", "package_info", "screenshot", "cache_clear", "scour_version"],
        "features": ["http+chrome ladder", "readability+fallback", "bm25 focus", "parallel batch", "in-domain crawl", "5-engine fused search", "search --fetch", "github/registry shortcuts", "ssrf guard", "fence-aware markdown cleanup", "links+page_type", "1hr cache", "keyless search", "screenshot", "offset pagination", "include_media", "actions", "pdf pages"],
        "browser": fetcher::browser_path().map(|p| p.display().to_string()).unwrap_or_else(|_| "none found".to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrape_request_deserializes_backwards_compat() {
        let json = r#"{"url":"https://example.com"}"#;
        let req: ScrapeRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.url, "https://example.com");
        assert!(req.offset.is_none());
        assert!(req.include_media.is_none());
        assert!(req.actions.is_none());
        assert!(req.pages.is_none());
    }

    #[test]
    fn action_deser_click_and_fill() {
        let j1 = r##"{"click":"#button"}"##;
        let a1: Action = serde_json::from_str(j1).unwrap();
        match a1 {
            Action::Click(s) => assert_eq!(s, "#button"),
            _ => panic!("wrong"),
        }
        let j2 = r##"{"fill":{"selector":"#input","text":"hello"}}"##;
        let a2: Action = serde_json::from_str(j2).unwrap();
        match a2 {
            Action::Fill { selector, text } => {
                assert_eq!(selector, "#input");
                assert_eq!(text, "hello");
            }
            _ => panic!("wrong"),
        }
    }
}
