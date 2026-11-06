//! HTML -> what an LLM can actually read: markdown, sanitized HTML, plain text.
//! Main-content root selection (article/main/known containers/readability/body), boilerplate
//! stripping before conversion, verbatim code blocks, and link + metadata extraction.

use anyhow::Result;
use scraper::node::Element;
use scraper::{ElementRef, Html, Node, Selector};
use std::sync::OnceLock;

static A_HREF_SEL: OnceLock<Selector> = OnceLock::new();
static TITLE_SEL: OnceLock<Selector> = OnceLock::new();
static META_DESC_SEL: OnceLock<Selector> = OnceLock::new();
static BODY_SEL: OnceLock<Selector> = OnceLock::new();

fn a_href_sel() -> &'static Selector {
    A_HREF_SEL.get_or_init(|| Selector::parse("a[href]").unwrap())
}
fn title_sel() -> &'static Selector {
    TITLE_SEL.get_or_init(|| Selector::parse("title").unwrap())
}
fn meta_desc_sel() -> &'static Selector {
    META_DESC_SEL.get_or_init(|| {
        Selector::parse(r#"meta[name="description"], meta[property="og:description"]"#).unwrap()
    })
}
fn body_sel() -> &'static Selector {
    BODY_SEL.get_or_init(|| Selector::parse("body").unwrap())
}

/// Main-content root candidates, highest priority first. Within a tier the best-scoring
/// candidate wins; the first tier with an acceptable candidate is used.
const ROOT_TIERS: &[&str] = &[
    "article",
    "main, [role=main]",
    "#mw-content-text, .mw-parser-output",
    "#content, .content, #main, .post, .entry-content",
    "#mainbar, #question",
];
/// Output budget (bytes of rendered HTML, ~markdown size) for a Wikipedia-style infobox table.
const INFOBOX_HTML: usize = 1500;
/// A root must have at least this much visible text to be chosen over the next candidate.
const MIN_ROOT_TEXT: usize = 200;
/// Cap on candidates rendered per tier (each candidate is walked in full).
const MAX_ROOT_CANDIDATES: usize = 10;

/// Tags html2md converts; everything else is unwrapped (children kept, tag dropped).
const KEEP_TAGS: &[&str] = &[
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "p",
    "ul",
    "ol",
    "li",
    "strong",
    "b",
    "em",
    "i",
    "code",
    "blockquote",
    "a",
];
/// Block containers, emitted as <p> so adjacent blocks don't run together.
const BLOCK_TAGS: &[&str] = &[
    "div",
    "section",
    "article",
    "main",
    "header",
    "figure",
    "figcaption",
    "details",
    "summary",
    "dl",
    "dt",
    "dd",
    "address",
];

/// Elements dropped together with their whole subtree.
const SKIP_TAGS: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "form", "input", "button", "select",
    "textarea", "nav", "footer", "aside", "iframe", "object", "embed", "canvas", "dialog", "link",
    "meta", "title", "base", "source", "track", "wbr", "col", "param", "area",
];
const ROLE_NOISE: &[&str] = &[
    "navigation",
    "banner",
    "contentinfo",
    "complementary",
    "search",
    "dialog",
];
/// Class/id words marking page furniture. Each class/id token is lowercased, `_` becomes `-`,
/// and it is wrapped in dashes, so only whole `-`-separated words match:
/// `vector-sidebar` and `sign_up` match, `navigator-free` does not.
const NOISE_WORDS: &[&str] = &[
    "-nav-",
    "-navbar-",
    "-navigation-",
    "-menu-",
    "-sidebar-",
    "-breadcrumb-",
    "-breadcrumbs-",
    "-footer-",
    "-cookie-",
    "-consent-",
    "-banner-",
    "-signup-",
    "-sign-up-",
    "-login-",
    "-modal-",
    "-popup-",
    "-newsletter-",
    "-share-",
    "-sharing-",
    "-social-",
    "-related-",
    "-advert-",
    "-ad-",
    "-ads-",
    "-promo-",
    "-toc-",
    "-mw-jump-link-",
    "-mw-editsection-",
    "-reflist-",
    "-references-",
    "-navbox-",
];

#[derive(Debug, serde::Serialize, Clone)]
pub struct Link {
    pub href: String,
    pub text: String,
}

#[derive(Debug, serde::Serialize)]
pub struct CleanDoc {
    pub title: String,
    pub markdown: String,
    pub clean_html: String,
    pub text: String,
    pub links: Vec<Link>,
    pub description: Option<String>,
    pub page_type: String,  // article | list | media | unknown
    pub quality_score: f32, // 0.0-1.0 rough heuristic
}

fn extract_links(doc: &Html, base_url: &str) -> Vec<Link> {
    let sel = a_href_sel();
    let base = url::Url::parse(base_url).ok();
    let mut links = Vec::new();
    for el in doc.select(sel) {
        let href = el.value().attr("href").unwrap_or("").trim();
        if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
            continue;
        }
        let abs = if let Some(b) = &base {
            b.join(href)
                .map(|u| u.to_string())
                .unwrap_or_else(|_| href.to_string())
        } else {
            href.to_string()
        };
        // Skip internal anchor duplicates and overly long hrefs
        if abs.len() > 2048 {
            continue;
        }
        let text = el.text().collect::<Vec<_>>().join(" ").trim().to_string();
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            continue;
        }
        // Cap at 100 chars for text
        let text = take_chars(&text, 100).to_string();
        links.push(Link { href: abs, text });
        if links.len() >= 100 {
            break;
        }
    }
    links
}

fn extract_meta(doc: &Html) -> (Option<String>, Option<String>) {
    let title = doc
        .select(title_sel())
        .next()
        .map(|e| e.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty());
    let desc = doc
        .select(meta_desc_sel())
        .next()
        .and_then(|e| e.value().attr("content"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    (title, desc)
}

/// True when the element is page furniture by tag, role, hidden flag, or class/id words.
fn is_skipped(e: &Element, name: &str, content_depth: u32) -> bool {
    if SKIP_TAGS.contains(&name) || (name == "header" && content_depth == 0) {
        return true;
    }
    if e.attr("hidden").is_some() || e.attr("aria-hidden") == Some("true") {
        return true;
    }
    if e.attr("role").is_some_and(|r| ROLE_NOISE.contains(&r)) {
        return true;
    }
    // Wikipedia citation markers ([85]); `a.reference` links elsewhere are real text.
    if name == "sup" && e.classes().any(|c| c == "reference") {
        return true;
    }
    // Code keeps its class-based markup (e.g. highlighter spans) and is never stripped.
    if matches!(name, "pre" | "code") {
        return false;
    }
    let hit = |v: Option<&str>| {
        v.is_some_and(|v| {
            v.split_whitespace().any(|tok| {
                let word = format!("-{}-", tok.replace('_', "-").to_ascii_lowercase());
                NOISE_WORDS.iter().any(|w| word.contains(w))
            })
        })
    };
    hit(e.attr("class")) || hit(e.attr("id"))
}

/// A code block's verbatim text plus its language (for the fence info string).
struct Code {
    lang: String,
    body: String,
}

/// Renders a chosen root to HTML for html2md: boilerplate dropped, each `<pre>` replaced by a
/// `<p>SCOURCODE{n}X</p>` placeholder (html2md would mangle whitespace), and the stats used
/// for root scoring and page_type.
#[derive(Default)]
struct Render {
    base: Option<url::Url>,
    html: String,
    plain: String,
    codes: Vec<Code>,
    text: usize,
    link_text: usize,
    p_text: usize,
    links: usize,
    link_depth: u32,
    p_depth: u32,
    /// Inside article/main, where a `<header>` is content rather than site chrome.
    content_depth: u32,
    /// Inside a `table.infobox`: output stops at this `html` length (facts, not essays).
    infobox_limit: Option<usize>,
}

impl Render {
    fn root(el: ElementRef, base: &url::Url) -> Self {
        let mut r = Self {
            base: Some(base.clone()),
            content_depth: u32::from(matches!(el.value().name(), "article" | "main")),
            ..Self::default()
        };
        r.children(el);
        r
    }

    fn children(&mut self, el: ElementRef) {
        for c in el.children() {
            match c.value() {
                Node::Text(t) => self.push_text(t),
                _ => {
                    if let Some(child) = ElementRef::wrap(c) {
                        self.element(child);
                    }
                }
            }
        }
    }

    fn push_text(&mut self, t: &str) {
        let trimmed = t.trim();
        let n = trimmed.len();
        self.text += n;
        if self.link_depth > 0 {
            self.link_text += n;
        }
        if self.p_depth > 0 {
            self.p_text += n;
        }
        if n > 0 {
            self.plain.push_str(trimmed);
            self.plain.push(' ');
        }
        escape_into(&mut self.html, t);
    }

    fn element(&mut self, el: ElementRef) {
        let e = el.value();
        let name = e.name();
        if is_skipped(e, name, self.content_depth)
            || self.infobox_limit.is_some_and(|l| self.html.len() >= l)
        {
            return;
        }
        if name == "table" && e.classes().any(|c| c == "infobox") {
            let outer = self.infobox_limit.replace(self.html.len() + INFOBOX_HTML);
            self.children(el);
            self.infobox_limit = outer;
            return;
        }
        match name {
            "pre" => return self.code_block(el),
            "br" | "hr" => {
                self.html.push_str(&format!("<{name}>"));
                return;
            }
            // Images without alt text are badges, spacers and placeholders: no text for an LLM.
            "img" => {
                let alt = e.attr("alt").unwrap_or("").trim();
                if !alt.is_empty()
                    && let Some(src) = e.attr("src").and_then(|s| self.abs(s))
                {
                    self.html.push_str("<img src=\"");
                    escape_into(&mut self.html, &src);
                    self.html.push_str("\" alt=\"");
                    escape_into(&mut self.html, alt);
                    self.html.push_str("\">");
                }
                return;
            }
            // Tables become one line per row with " | " between cells: html2md's table
            // output turns layout tables (HN, infoboxes) into unreadable pipe junk.
            "table" | "thead" | "tbody" | "tfoot" | "caption" => return self.children(el),
            "tr" => {
                self.html.push_str("<p>");
                self.children(el);
                self.html.push_str("</p>");
                return;
            }
            "td" | "th" => {
                if is_blank(el) {
                    return;
                }
                let earlier_cell = el
                    .prev_siblings()
                    .filter_map(ElementRef::wrap)
                    .any(|c| !is_blank(c));
                if earlier_cell {
                    self.html.push_str(" | ");
                }
                return self.children(el);
            }
            _ => {}
        }
        // html2md knows only these tags; it would print any other tag as literal text.
        // Block containers become <p> so their contents stay on separate lines.
        let out = if KEEP_TAGS.contains(&name) {
            name
        } else if BLOCK_TAGS.contains(&name) {
            "p"
        } else {
            return self.children(el);
        };
        // Blocks inside <a> make html5ever split or empty the link; keep them as spaced text.
        let block_in_link = matches!(
            out,
            "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "ul" | "ol" | "li" | "blockquote"
        );
        if block_in_link && self.link_depth > 0 {
            self.html.push(' ');
            self.children(el);
            self.html.push(' ');
            return;
        }
        let is_p = name == "p";
        let is_content = matches!(name, "article" | "main");
        // `<a>` without an href (fragment links like `#heading`) is just its text.
        let href = if name == "a" {
            e.attr("href").and_then(|h| self.abs(h))
        } else {
            None
        };
        let is_link = href.is_some();
        if name == "a" && !is_link {
            return self.children(el);
        }
        let mark = self.html.len();
        let text_before = self.text;
        let links_before = self.links;
        self.html.push('<');
        self.html.push_str(out);
        if let Some(href) = href {
            self.html.push_str(" href=\"");
            escape_into(&mut self.html, &href);
            self.html.push('"');
        }
        if is_link {
            self.link_depth += 1;
            self.links += 1;
        }
        self.html.push('>');
        self.p_depth += u32::from(is_p);
        self.content_depth += u32::from(is_content);
        self.children(el);
        self.p_depth -= u32::from(is_p);
        self.content_depth -= u32::from(is_content);
        if is_link {
            self.link_depth -= 1;
        }
        // No visible text inside (icon/badge links, empty headings, logo wrappers): drop it whole.
        if self.text == text_before {
            self.html.truncate(mark);
            self.links = links_before;
            return;
        }
        self.html.push_str(&format!("</{out}>"));
    }

    fn code_block(&mut self, el: ElementRef) {
        let mut raw = String::new();
        code_text(el, &mut raw);
        let raw = raw.replace('\r', "");
        let body = raw.trim_matches('\n');
        if body.trim().is_empty() {
            return;
        }
        self.text += body.trim().len();
        self.plain.push_str(body);
        self.plain.push(' ');
        self.codes.push(Code {
            lang: code_lang(el),
            body: body.to_string(),
        });
        self.html
            .push_str(&format!("<p>SCOURCODE{}X</p>", self.codes.len() - 1));
    }

    /// Absolute URL for links/images; None for empty, fragment, data:, and javascript: values.
    fn abs(&self, v: &str) -> Option<String> {
        let v = v.trim();
        if v.is_empty()
            || v.starts_with('#')
            || v.starts_with("data:")
            || v.starts_with("javascript:")
        {
            return None;
        }
        let abs = match &self.base {
            Some(b) => b.join(v).ok()?.to_string(),
            None => v.to_string(),
        };
        (abs.len() <= 2048).then_some(abs)
    }

    fn score(&self) -> i64 {
        self.text as i64 - 2 * self.link_text as i64
    }
}

/// True when an element has no non-whitespace text anywhere below it.
fn is_blank(el: ElementRef) -> bool {
    el.text().all(|t| t.trim().is_empty())
}

/// Raw text of a `<pre>`: all descendant text verbatim, `<br>` and block boundaries as newlines.
/// Copy buttons and other UI inside code are dropped.
fn code_text(el: ElementRef, out: &mut String) {
    for c in el.children() {
        match c.value() {
            Node::Text(t) => out.push_str(t),
            _ => {
                let Some(child) = ElementRef::wrap(c) else {
                    continue;
                };
                let name = child.value().name();
                match name {
                    "br" => out.push('\n'),
                    "script" | "style" | "button" | "svg" | "noscript" | "template" => {}
                    _ => {
                        let block = matches!(name, "div" | "p" | "li" | "tr");
                        if block && !out.is_empty() && !out.ends_with('\n') {
                            out.push('\n');
                        }
                        code_text(child, out);
                    }
                }
            }
        }
    }
}

/// Language from `language-x` / `lang-x` / `highlight-x` on the pre, its parent, or its code child.
fn code_lang(el: ElementRef) -> String {
    let mut classes: Vec<&str> = el.value().classes().collect();
    if let Some(p) = el.parent().and_then(ElementRef::wrap) {
        classes.extend(p.value().classes());
    }
    if let Some(c) = el
        .descendent_elements()
        .find(|e| e.value().name() == "code")
    {
        classes.extend(c.value().classes());
    }
    for c in classes {
        for prefix in ["language-", "lang-", "highlight-"] {
            if let Some(l) = c.strip_prefix(prefix) {
                let l = l.strip_prefix("source-").unwrap_or(l);
                if !l.is_empty()
                    && l.chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || "+-_#.".contains(ch))
                {
                    return l.to_string();
                }
            }
        }
    }
    String::new()
}

fn escape_into(out: &mut String, s: &str) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
}

/// First `n` chars of `s`. Never splits a UTF-8 code point.
fn take_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Cleanup for html2md output. Single pass, O(n):
/// - runs of 3+ spaces/tabs inside a line collapse to one space (leading indent kept)
/// - lines made only of `|`, `-`, `:` are dropped, except a `|---|` separator right after a text line
/// - 3+ consecutive blank lines collapse to one blank line; leading/trailing blanks are dropped
pub fn clean_markdown(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut blanks = 0usize; // pending blank lines, not yet written
    let mut prev_text = false; // previous input line had alphanumerics
    let mut prev_indent_code = false; // previous line was indented code
    let mut fence: Option<u8> = None; // inside ``` / ~~~ block (which marker)
    for line in md.lines() {
        // Fenced code: emit verbatim so alignment and rule-like lines survive.
        if let Some(ch) = fence {
            flush_blanks(&mut out, &mut blanks);
            out.push_str(line);
            out.push('\n');
            if fence_marker(line) == Some(ch) {
                fence = None;
            }
            prev_text = false;
            prev_indent_code = false;
            continue;
        }
        let lead = line.len() - line.trim_start().len();
        // Opening fence (indent < 4, else it is indented code).
        if lead < 4
            && let Some(ch) = fence_marker(line)
        {
            flush_blanks(&mut out, &mut blanks);
            out.push_str(line);
            out.push('\n');
            fence = Some(ch);
            prev_text = false;
            prev_indent_code = false;
            continue;
        }
        let t = line.trim();
        if t.is_empty() {
            blanks += 1;
            prev_text = false;
            prev_indent_code = false;
            continue;
        }
        // Indented code block: >=4 spaces/tab after a blank line or more code.
        if lead >= 4 && (blanks > 0 || prev_indent_code) {
            flush_blanks(&mut out, &mut blanks);
            out.push_str(line);
            out.push('\n');
            prev_text = false;
            prev_indent_code = true;
            continue;
        }
        let has_text = t.chars().any(char::is_alphanumeric);
        let rule_like = t
            .chars()
            .all(|c| c.is_whitespace() || matches!(c, '|' | '-' | ':'));
        let separator_after_text = prev_text && t.contains('|') && t.contains('-');
        prev_text = has_text;
        prev_indent_code = false;
        if rule_like && !separator_after_text {
            continue;
        }
        flush_blanks(&mut out, &mut blanks);
        out.push_str(&line[..lead]);
        collapse_space_runs(&line[lead..], &mut out);
        out.push('\n');
    }
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Write pending blank lines (3+ collapsed to one), then reset the counter.
fn flush_blanks(out: &mut String, blanks: &mut usize) {
    if !out.is_empty() {
        let n = if *blanks >= 3 { 1 } else { *blanks };
        out.push_str(&"\n".repeat(n));
    }
    *blanks = 0;
}

/// Fence marker a line opens/closes with (ignoring up-to-3-space indent).
fn fence_marker(line: &str) -> Option<u8> {
    let t = line.trim_start();
    if t.starts_with("```") {
        Some(b'`')
    } else if t.starts_with("~~~") {
        Some(b'~')
    } else {
        None
    }
}

/// Append `s` to `out`, replacing each run of 3+ spaces/tabs with one space.
/// Slices only at ASCII boundaries, so it is UTF-8 safe.
fn collapse_space_runs(s: &str, out: &mut String) {
    let b = s.as_bytes();
    let (mut i, mut last) = (0, 0);
    while i < b.len() {
        if b[i] == b' ' || b[i] == b'\t' {
            let start = i;
            while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
                i += 1;
            }
            if i - start > 2 {
                out.push_str(&s[last..start]);
                out.push(' ');
                last = i;
            }
        } else {
            i += 1;
        }
    }
    out.push_str(&s[last..]);
}

fn is_table_artifact(md: &str) -> bool {
    let pipe_count = md.matches('|').count();
    let alpha: usize = md.chars().filter(|c| c.is_alphabetic()).count();
    // HN-style layout tables produce huge pipes with lots of whitespace
    pipe_count > 20 && alpha > 0 && (pipe_count as f32 / alpha as f32) > 0.05 && md.contains("|  ")
}

fn generate_link_markdown(title: &str, links: &[Link], body_text: &str) -> String {
    // For list pages where html2md produced table garbage, build readable markdown from links + body text
    let body_text: String = body_text.split_whitespace().collect::<Vec<_>>().join(" ");
    let body_snippet = take_chars(&body_text, 2000);
    // Deduplicate links by href, keep order
    let mut seen = std::collections::HashSet::new();
    let mut unique: Vec<&Link> = Vec::new();
    for l in links {
        if seen.insert(&l.href) {
            unique.push(l);
        }
    }
    let link_lines: Vec<String> = unique
        .iter()
        .take(50)
        .map(|l| format!("- [{}]({})", l.text, l.href))
        .collect();
    if title.is_empty() {
        format!("{}\n\n{}", body_snippet, link_lines.join("\n"))
    } else {
        format!(
            "# {}\n\n{}\n\n{}",
            title,
            body_snippet,
            link_lines.join("\n")
        )
    }
}

/// article: paragraph text dominates. list: many links carry ≥40% of the text and there is
/// little paragraph prose (HN, index pages). media: images with little text.
fn page_type(r: &Render, markdown: &str) -> String {
    let link_heavy = r.links >= 10 && r.link_text * 5 >= r.text * 2;
    if link_heavy && r.p_text < 200 {
        return "list".to_string();
    }
    if markdown.contains("![") && r.text < 500 {
        return "media".to_string();
    }
    if r.text >= 100 {
        return "article".to_string();
    }
    "unknown".to_string()
}

fn quality_score(text_len: usize, html_len: usize, links_len: usize, title_len: usize) -> f32 {
    if text_len == 0 {
        return 0.0;
    }
    let text_ratio = text_len as f32 / html_len.max(1) as f32;
    let mut score = (text_ratio * 5.0).min(1.0) * 0.4;
    score += (text_len as f32 / 5000.0).min(1.0) * 0.3;
    score += if title_len > 5 { 0.15 } else { 0.0 };
    score += if links_len > 0 && links_len < 50 {
        0.15
    } else {
        0.0
    };
    score.min(1.0)
}

/// Fenced block for a code body. Fence is longer than any backtick run in the body.
fn fenced(c: &Code) -> String {
    let longest = c
        .body
        .split(|ch| ch != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("\n\n{fence}{}\n{}\n{fence}\n\n", c.lang, c.body)
}

/// html2md leaks escaped markup from raw-text elements as lines like `\<style\>...`.
fn is_escaped_markup(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    l.contains("\\<style") || l.contains("\\<script")
}

fn to_markdown(r: &Render, title: &str, links: &[Link]) -> String {
    let md = html2md::parse_html(&r.html);
    let md = md
        .lines()
        .filter(|l| !is_escaped_markup(l))
        .collect::<Vec<_>>()
        .join("\n");
    // Detect table-layout artifact (HN, old forums) and replace with link-based markdown
    if is_table_artifact(&md) {
        return generate_link_markdown(title, links, &r.plain);
    }
    let mut md = md;
    for (i, c) in r.codes.iter().enumerate() {
        md = md.replace(&format!("SCOURCODE{i}X"), &fenced(c));
    }
    clean_markdown(&md)
}

/// Sanitized-HTML view of the root: placeholders become real `<pre><code>` blocks.
fn fill_html(html: &str, codes: &[Code]) -> String {
    let mut out = html.to_string();
    for (i, c) in codes.iter().enumerate() {
        let mut pre = String::from("<pre><code>");
        escape_into(&mut pre, &c.body);
        pre.push_str("</code></pre>");
        out = out.replace(&format!("<p>SCOURCODE{i}X</p>"), &pre);
    }
    out
}

/// Readability's article HTML, re-parsed as a fragment (one root candidate among several).
fn readability_fragment(html: &str, base: &url::Url) -> Option<Html> {
    let article =
        readability::extractor::extract(&mut std::io::Cursor::new(html.as_bytes()), base).ok()?;
    Some(Html::parse_fragment(&article.content))
}

/// Chooses the main-content root: first tier with an acceptable candidate (best score within
/// the tier), then readability's content (run only if the tiers fail), then `<body>`.
fn pick_root(doc: &Html, readability: impl FnOnce() -> Option<Html>, base: &url::Url) -> Render {
    for tier in ROOT_TIERS {
        let sel = Selector::parse(tier).unwrap();
        let mut best: Option<Render> = None;
        for el in doc.select(&sel).take(MAX_ROOT_CANDIDATES) {
            let r = Render::root(el, base);
            if r.text >= MIN_ROOT_TEXT && best.as_ref().is_none_or(|b| r.score() > b.score()) {
                best = Some(r);
            }
        }
        if let Some(r) = best {
            return r;
        }
    }
    if let Some(rd) = readability() {
        let r = Render::root(rd.root_element(), base);
        if r.text >= MIN_ROOT_TEXT {
            return r;
        }
    }
    let el = doc
        .select(body_sel())
        .next()
        .unwrap_or_else(|| doc.root_element());
    Render::root(el, base)
}

pub fn extract(html: &str, url: &str) -> Result<CleanDoc> {
    let base = url::Url::parse(url)?;
    let doc = Html::parse_document(html);
    let links = extract_links(&doc, url);
    let (title, description) = extract_meta(&doc);
    let title = title.unwrap_or_default();

    let root = pick_root(&doc, || readability_fragment(html, &base), &base);
    let markdown = to_markdown(&root, &title, &links);
    let text = root.plain.split_whitespace().collect::<Vec<_>>().join(" ");
    let clean_html = ammonia::clean(&fill_html(&root.html, &root.codes));
    let ptype = page_type(&root, &markdown);
    let q = quality_score(text.len(), html.len(), links.len(), title.len());

    Ok(CleanDoc {
        title,
        markdown,
        clean_html,
        text,
        links,
        description,
        page_type: ptype,
        quality_score: q,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn strips_scripts_and_makes_markdown() {
        let html = format!(
            "<html><head><title>T</title></head><body><article><h1>Hello</h1>\
             <script>alert(1)</script><p>{}</p></article></body></html>",
            "real body text ".repeat(40)
        );
        let d = super::extract(&html, "https://example.com/a").unwrap();
        assert!(
            !d.clean_html.contains("<script"),
            "script survived sanitizer"
        );
        assert!(d.markdown.contains("real body text"));
        assert!(d.text.contains("real body text"));
    }

    #[test]
    fn fallback_captures_list_pages() {
        // Simulate HN-like list page: readability would extract little, but fallback should capture links
        let mut html = String::from("<html><head><title>Hacker News</title></head><body>");
        for i in 0..30 {
            html.push_str(&format!(r#"<div><a href="https://example.com/{i}">Article {i}</a> <span>{} points</span></div>"#, i*10));
        }
        html.push_str("</body></html>");
        let d = super::extract(&html, "https://news.ycombinator.com").unwrap();
        assert!(
            d.links.len() >= 20,
            "should extract many links, got {}",
            d.links.len()
        );
        assert!(
            d.page_type == "list" || d.markdown.len() > 500,
            "should be list or have substantial markdown, got type={} len={}",
            d.page_type,
            d.markdown.len()
        );
    }

    #[test]
    fn short_article_not_misclassified_as_list() {
        let html = r#"<html><head><title>Example Domain</title></head><body><h1>Example Domain</h1><p>This domain is for use in documentation examples without needing permission. Avoid use in operations.</p><p><a href="https://iana.org/domains/example">Learn more</a></p></body></html>"#;
        let d = super::extract(html, "https://example.com").unwrap();
        assert_eq!(d.page_type, "article");
        assert!(d.title.contains("Example Domain") || d.markdown.contains("Example Domain"));
        assert!(d.markdown.contains("This domain"));
    }

    #[test]
    fn truncates_multibyte_text_by_chars() {
        let long_cjk = "日本語".repeat(100); // 300 chars, 900 bytes
        assert_eq!(super::take_chars(&long_cjk, 100).chars().count(), 100);
        assert_eq!(super::take_chars("短い", 100), "短い");
        let html = format!(r#"<a href="https://example.com/x">{long_cjk}</a>"#);
        let links =
            super::extract_links(&super::Html::parse_document(&html), "https://example.com");
        assert_eq!(links[0].text.chars().count(), 100);
        let body = "中文テキスト ".repeat(500);
        let md = super::generate_link_markdown("T", &[], &body);
        assert!(
            md.chars().count() < 2100,
            "snippet not capped: {}",
            md.chars().count()
        );
    }

    #[test]
    fn clean_markdown_collapses_space_runs() {
        assert_eq!(super::clean_markdown("a      b"), "a b");
        assert_eq!(super::clean_markdown("a  b"), "a  b");
        assert_eq!(super::clean_markdown("- x\n    - y"), "- x\n    - y");
        let huge = format!("x{}y", " ".repeat(1_000_000));
        assert_eq!(super::clean_markdown(&huge), "x y");
    }

    #[test]
    fn clean_markdown_drops_rule_junk_but_keeps_tables() {
        assert_eq!(super::clean_markdown("text\n|  |  |\nmore"), "text\nmore");
        assert_eq!(super::clean_markdown("|---|---|\nafter"), "after");
        let table = "| a | b |\n|---|:--:|\n| 1 | 2 |";
        assert_eq!(super::clean_markdown(table), table);
    }

    #[test]
    fn clean_markdown_preserves_fenced_code() {
        let md =
            "intro\n\n```rust\nfn main() {\n    let x   = 1;\n    ------\n}\n```\n\nafter   text";
        let out = super::clean_markdown(md);
        assert!(
            out.contains("    let x   = 1;"),
            "aligned code collapsed: {out:?}"
        );
        assert!(
            out.contains("    ------"),
            "rule inside fence dropped: {out:?}"
        );
        assert!(
            out.contains("after text"),
            "prose runs not collapsed: {out:?}"
        );
    }

    #[test]
    fn clean_markdown_preserves_indented_code() {
        let md = "prose here\n\n    code    aligned\n    ------\n\nmore   prose";
        let out = super::clean_markdown(md);
        assert!(
            out.contains("    code    aligned"),
            "indented code collapsed: {out:?}"
        );
        assert!(
            out.contains("    ------"),
            "rule in indented code dropped: {out:?}"
        );
        assert!(out.contains("more prose"));
    }

    #[test]
    fn clean_markdown_collapses_blank_runs() {
        assert_eq!(super::clean_markdown("a\n\n\n\nb"), "a\n\nb");
        assert_eq!(super::clean_markdown("\n\na\n\n"), "a");
    }

    #[test]
    fn clean_markdown_is_utf8_safe() {
        let md = "日本語   テキスト\n\n\n\n|---|\n中文 ✓   ok";
        assert_eq!(super::clean_markdown(md), "日本語 テキスト\n\n中文 ✓ ok");
    }
}

#[cfg(test)]
mod main_content_tests {
    use super::extract;

    /// Real pages from tests/fixtures/extract (scripts/styles stripped, big ones cut to 300KB).
    /// Columns: file, url, phrase expected near the top, page_type.
    const PAGES: &[(&str, &str, &str, &str, &str)] = &[
        (
            include_str!("../tests/fixtures/extract/wikipedia_rust.html"),
            "wikipedia",
            "https://en.wikipedia.org/wiki/Rust_(programming_language)",
            "**Rust** is a",
            "article",
        ),
        (
            include_str!("../tests/fixtures/extract/mdn_cache_control.html"),
            "mdn",
            "https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Cache-Control",
            "Cache-Control",
            "article",
        ),
        (
            include_str!("../tests/fixtures/extract/python_asyncio_task.html"),
            "python",
            "https://docs.python.org/3/library/asyncio-task.html",
            "Coroutines",
            "article",
        ),
        (
            include_str!("../tests/fixtures/extract/bbc_news.html"),
            "bbc",
            "https://www.bbc.com/news",
            "LIVE",
            // Front page is a headline index, so "list" like HN.
            "list",
        ),
        (
            include_str!("../tests/fixtures/extract/hn_front.html"),
            "hn",
            "https://news.ycombinator.com/",
            "Hacker News",
            "list",
        ),
        (
            include_str!("../tests/fixtures/extract/react_useeffect.html"),
            "react",
            "https://react.dev/reference/react/useEffect",
            "useEffect",
            "article",
        ),
        (
            include_str!("../tests/fixtures/extract/github_axum.html"),
            "github",
            "https://github.com/tokio-rs/axum",
            "HTTP routing",
            "article",
        ),
        (
            // Blog index: a list of post links, so "list" is the right type.
            include_str!("../tests/fixtures/extract/blog_rust_lang.html"),
            "blog",
            "https://blog.rust-lang.org/",
            "Announcing",
            "list",
        ),
        (
            include_str!("../tests/fixtures/extract/arxiv_attention.html"),
            "arxiv",
            "https://arxiv.org/abs/1706.03762",
            "Attention Is All You Need",
            "article",
        ),
        (
            include_str!("../tests/fixtures/extract/stackoverflow_rust_package_manager.html"),
            "stackoverflow",
            "https://stackoverflow.com/questions/26946646/package-manager-for-rust",
            "reusable library",
            "article",
        ),
    ];

    #[test]
    fn pages_lead_with_content_and_keep_their_type() {
        for (html, name, url, phrase, ptype) in PAGES {
            let d = extract(html, url).unwrap();
            // Wikipedia's infobox is capped but still precedes the lead, so it gets a wider window.
            let window = if *name == "wikipedia" { 2500 } else { 400 };
            let head: String = d.markdown.chars().take(window).collect();
            assert!(
                head.contains(phrase),
                "{name}: {phrase:?} not near top: {head:?}"
            );
            assert_eq!(d.page_type, *ptype, "{name}");
            for junk in [
                "Main menu",
                "Jump to content",
                "Skip to main content",
                "Sign up",
                "Log in",
                "\\<style",
                "[](",
            ] {
                assert!(!d.markdown.contains(junk), "{name}: found {junk:?}");
            }
        }
    }

    #[test]
    fn wikipedia_markdown_is_size_capped() {
        let html = include_str!("../tests/fixtures/extract/wikipedia_rust.html");
        let d = extract(
            html,
            "https://en.wikipedia.org/wiki/Rust_(programming_language)",
        )
        .unwrap();
        assert!(d.markdown.len() < 150_000, "got {}", d.markdown.len());
    }

    #[test]
    fn python_code_keeps_spaces_and_lines() {
        let html = include_str!("../tests/fixtures/extract/python_asyncio_task.html");
        let d = extract(html, "https://docs.python.org/3/library/asyncio-task.html").unwrap();
        assert!(
            d.markdown.contains(">>> import asyncio\n"),
            "{}",
            d.markdown
        );
        assert!(
            d.markdown
                .contains(">>> async def main():\n...     print('hello')"),
            "{}",
            d.markdown
        );
        assert!(!d.markdown.contains("importasyncio"));
    }

    #[test]
    fn pre_keeps_text_verbatim_with_language() {
        let html = "<html><body><article><p>Intro text that is long enough to be the main content of this page.</p>\
            <div class=\"highlight-python3\"><pre><span class=\"gp\">&gt;&gt;&gt; </span><span>import</span> <span>asyncio</span>\n    \
            <span>    x = 1</span><br>y<button>Copy</button></pre></div></article></body></html>";
        let d = extract(html, "https://example.com/").unwrap();
        assert!(
            d.markdown
                .contains("```python3\n>>> import asyncio\n        x = 1\ny\n```"),
            "{}",
            d.markdown
        );
        assert!(!d.markdown.contains("Copy"), "{}", d.markdown);
    }

    #[test]
    fn strips_page_furniture_outside_and_inside_main() {
        let body = "Real body text. ".repeat(30);
        let html = format!(
            r##"<html><head><title>Guide</title></head><body>
            <header><a href="/">Site</a> <a href="/login">Log in</a></header>
            <nav><a href="/n">Nav link</a></nav>
            <div class="sidebar-left"><a href="/s">Side link</a></div>
            <div id="cookie-banner">We use cookies</div>
            <main><h1>Guide</h1><p>{body}</p><div class="ad-slot">Buy now</div>
            <div class="reflist">Ref one</div><a href="#top">Top</a></main>
            <footer>Copyright</footer></body></html>"##
        );
        let d = extract(&html, "https://example.com/guide").unwrap();
        for junk in [
            "Log in",
            "Nav link",
            "Side link",
            "cookies",
            "Buy now",
            "Ref one",
            "Copyright",
            "Site",
        ] {
            assert!(!d.markdown.contains(junk), "{junk:?} kept: {}", d.markdown);
        }
        assert!(d.markdown.starts_with("# Guide") || d.markdown.contains("Guide\n"));
        assert!(d.markdown.contains("Real body text"));
        assert_eq!(d.title, "Guide");
    }
}
