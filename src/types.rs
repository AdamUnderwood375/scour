//! Shared data types used across the scrape pipeline, CLI and MCP server.

#[derive(Debug, serde::Deserialize, schemars::JsonSchema, Clone)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Action {
    Click(String),
    Fill { selector: String, text: String },
    Press(String),
    Wait(u64),
    Scroll(u32),
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema, Clone)]
pub(crate) struct LinkOut {
    pub href: String,
    pub text: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema, Clone)]
pub(crate) struct ScrapeResult {
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
    /// Chars of the text `offset` indexes into (focused markdown, before max_chars).
    /// Used by the truncation hint; `total_chars` is the whole page.
    #[serde(default)]
    #[schemars(description = "Chars of the focused text that offset indexes into")]
    pub focused_chars: usize,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct CrawlResult {
    pub start_url: String,
    pub pages: Vec<ScrapeResult>,
    pub visited: Vec<String>,
    pub total_chars: usize,
    /// Pages whose fetch failed. Not serialized here; the CLI prints them.
    #[serde(skip)]
    pub failed: Vec<(String, anyhow::Error)>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema, Clone)]
pub(crate) struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct SearchResult {
    pub query: String,
    pub hits: Vec<SearchHit>,
    pub total: usize,
    /// Per-engine status, e.g. ["ddg:ok:10","bing:ok:9","brave:blocked:captcha","mojeek:timeout"].
    #[serde(default)]
    pub engines: Vec<String>,
    /// Low-relevance / degraded-coverage advisory (off-topic top hits or <2 ok engines).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}
