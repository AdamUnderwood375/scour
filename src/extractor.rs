//! HTML -> what an LLM can actually read: markdown, sanitized HTML, plain text.
//! Frontier upgrade: readability-first with trafilatura-style fallback for list/index pages,
//! plus link extraction, metadata, and page-type detection.

use anyhow::Result;
use scraper::{Html, Selector};
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
    META_DESC_SEL.get_or_init(|| Selector::parse(r#"meta[name="description"], meta[property="og:description"]"#).unwrap())
}
fn body_sel() -> &'static Selector {
    BODY_SEL.get_or_init(|| Selector::parse("body").unwrap())
}

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
    pub page_type: String, // article | list | media | unknown
    pub quality_score: f32, // 0.0-1.0 rough heuristic
}

fn visible_text_len(html: &str) -> usize {
    // Fast strip tags to estimate visible text for fallback decision
    let mut len = 0;
    let mut in_tag = false;
    let mut in_script = false;
    let lower = html.to_lowercase();
    // crude but fast: count non-tag, non-script chars
    let mut i = 0;
    let b = html.as_bytes();
    let lb = lower.as_bytes();
    while i < b.len() {
        if lb[i..].starts_with(b"<script") {
            in_script = true;
            i += 7;
            continue;
        }
        if in_script && lb[i..].starts_with(b"</script") {
            in_script = false;
            i += 8;
            continue;
        }
        if lb[i..].starts_with(b"<style") {
            in_script = true;
            i += 6;
            continue;
        }
        if in_script && lb[i..].starts_with(b"</style") {
            in_script = false;
            i += 7;
            continue;
        }
        if b[i] == b'<' {
            in_tag = true;
        }
        if !in_tag && !in_script && b[i] != b'>' {
            if !b[i].is_ascii_whitespace() {
                len += 1;
            }
        }
        if b[i] == b'>' {
            in_tag = false;
        }
        i += 1;
    }
    len
}

fn extract_links(html: &str, base_url: &str) -> Vec<Link> {
    let doc = Html::parse_document(html);
    let sel = a_href_sel();
    let base = url::Url::parse(base_url).ok();
    let mut links = Vec::new();
    for el in doc.select(&sel) {
        let href = el.value().attr("href").unwrap_or("").trim();
        if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
            continue;
        }
        let abs = if let Some(b) = &base {
            b.join(href).map(|u| u.to_string()).unwrap_or_else(|_| href.to_string())
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
        let text = if text.len() > 100 { text[..100].to_string() } else { text };
        links.push(Link { href: abs, text });
        if links.len() >= 100 {
            break;
        }
    }
    links
}

fn extract_meta(html: &str) -> (Option<String>, Option<String>) {
    let doc = Html::parse_document(html);
    let title = doc.select(title_sel()).next().map(|e| e.text().collect::<String>().trim().to_string()).filter(|s| !s.is_empty());
    let desc = doc.select(meta_desc_sel()).next().and_then(|e| e.value().attr("content")).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    (title, desc)
}

fn is_table_artifact(md: &str) -> bool {
    let pipe_count = md.matches('|').count();
    let alpha: usize = md.chars().filter(|c| c.is_alphabetic()).count();
    // HN-style layout tables produce huge pipes with lots of whitespace
    pipe_count > 20 && alpha > 0 && (pipe_count as f32 / alpha as f32) > 0.05 && md.contains("|  ")
}

fn generate_link_markdown(title: &str, links: &[Link], html: &str) -> String {
    // For list pages where html2md produced table garbage, build readable markdown from links + extracted body text
    let doc = Html::parse_document(html);
    // Extract visible body text (first 2000 chars, collapsed whitespace)
    let body_text = doc
        .select(body_sel())
        .next()
        .map(|b| b.text().collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    let body_text: String = body_text.split_whitespace().collect::<Vec<_>>().join(" ");
    let body_snippet = if body_text.len() > 2000 {
        body_text[..2000].to_string()
    } else {
        body_text
    };
    // Deduplicate links by href, keep order
    let mut seen = std::collections::HashSet::new();
    let mut unique: Vec<&Link> = Vec::new();
    for l in links {
        if seen.insert(&l.href) {
            unique.push(l);
        }
    }
    let link_lines: Vec<String> = unique.iter().take(50).map(|l| format!("- [{}]({})", l.text, l.href)).collect();
    if title.is_empty() {
        format!("{}\n\n{}", body_snippet, link_lines.join("\n"))
    } else {
        format!("# {}\n\n{}\n\n{}", title, body_snippet, link_lines.join("\n"))
    }
}

fn page_type(markdown: &str, links: &[Link], text_len: usize, html_len: usize) -> String {
    let link_density = if text_len > 0 { links.len() as f32 / (text_len as f32 / 100.0) } else { 0.0 };
    let has_list_markers = markdown.matches("\n- ").count() + markdown.matches("\n* ").count() + markdown.matches("\n1. ").count();
    let has_headings = markdown.matches("\n#").count();

    if html_len < 2000 && text_len < 800 {
        return "article".to_string();
    }
    if links.len() > 20 && (link_density > 1.5 || has_list_markers > 10 || has_headings == 0) {
        return "list".to_string();
    }
    if has_headings == 0 && links.len() > 15 && text_len < 5000 {
        return "list".to_string();
    }
    if markdown.contains("![") && text_len < 500 {
        return "media".to_string();
    }
    if text_len > 500 && has_headings > 0 {
        return "article".to_string();
    }
    if text_len > 300 {
        return "article".to_string();
    }
    "unknown".to_string()
}

fn quality_score(text_len: usize, html_len: usize, links_len: usize, title_len: usize) -> f32 {
    if text_len == 0 { return 0.0; }
    let text_ratio = text_len as f32 / html_len.max(1) as f32;
    let mut score = (text_ratio * 5.0).min(1.0) * 0.4;
    score += (text_len as f32 / 5000.0).min(1.0) * 0.3;
    score += if title_len > 5 { 0.15 } else { 0.0 };
    score += if links_len > 0 && links_len < 50 { 0.15 } else { 0.0 };
    score.min(1.0)
}

pub fn extract(html: &str, url: &str) -> Result<CleanDoc> {
    let base = url::Url::parse(url)?;
    let links = extract_links(html, url);
    let mut cursor = std::io::Cursor::new(html.as_bytes());

    // Try readability first (best for articles)
    let readability_result = readability::extractor::extract(&mut cursor, &base);

    let (raw_title, raw_content, raw_text) = match readability_result {
        Ok(p) => (p.title, p.content, p.text),
        Err(_) => (String::new(), html.to_string(), String::new()),
    };

    let clean_html_primary = ammonia::clean(&raw_content);
    let markdown_primary = html2md::parse_html(&clean_html_primary);
    let text_primary = raw_text.clone();

    // Decide if we need fallback: readability produced thin content but page has substantial visible text
    let visible_len = visible_text_len(html);
    let primary_text_len = text_primary.trim().len();
    let needs_fallback = primary_text_len < 300 && visible_len > 800
        || markdown_primary.trim().len() < 200 && visible_len > 600
        || primary_text_len < (visible_len as f64 * 0.25) as usize && visible_len > 1000;

    let (mut title, mut markdown, mut clean_html, mut text) = if needs_fallback {
        // Fallback: clean full HTML and convert – captures lists, feeds, index pages
        let fallback_clean = ammonia::clean(html);
        let mut fallback_md = html2md::parse_html(&fallback_clean);
        // Extract title from raw HTML if readability title empty
        let (meta_title, _) = extract_meta(html);
        let fb_title = if !raw_title.trim().is_empty() {
            raw_title.clone()
        } else {
            meta_title.unwrap_or_default()
        };
        // Detect table-layout artifact (HN, old forums) and replace with link-based markdown
        if is_table_artifact(&fallback_md) {
            fallback_md = generate_link_markdown(&fb_title, &links, &fallback_clean);
        }
        let fb_text = fallback_md.clone();
        if fallback_md.trim().len() > markdown_primary.trim().len() * 2
            || primary_text_len < 100 && fallback_md.trim().len() > 300
        {
            (fb_title, fallback_md, fallback_clean, fb_text)
        } else {
            (raw_title, markdown_primary, clean_html_primary, text_primary)
        }
    } else {
        (raw_title, markdown_primary, clean_html_primary, text_primary)
    };

    // Post-check: even when we didn't fallback, if markdown is still table garbage, fix it
    if is_table_artifact(&markdown) {
        markdown = generate_link_markdown(&title, &links, &clean_html);
        text = markdown.clone();
    }

    let (_, description) = extract_meta(html);
    let ptype = page_type(&markdown, &links, text.trim().len(), html.len());
    let q = quality_score(text.trim().len(), html.len(), links.len(), title.len());

    // Use meta title fallback if still empty
    let final_title = if title.trim().is_empty() {
        extract_meta(html).0.unwrap_or_default()
    } else {
        title
    };

    Ok(CleanDoc {
        title: final_title,
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
        assert!(!d.clean_html.contains("<script"), "script survived sanitizer");
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
        assert!(d.links.len() >= 20, "should extract many links, got {}", d.links.len());
        assert!(d.page_type == "list" || d.markdown.len() > 500, "should be list or have substantial markdown, got type={} len={}", d.page_type, d.markdown.len());
    }

    #[test]
    fn short_article_not_misclassified_as_list() {
        let html = r#"<html><head><title>Example Domain</title></head><body><h1>Example Domain</h1><p>This domain is for use in documentation examples without needing permission. Avoid use in operations.</p><p><a href="https://iana.org/domains/example">Learn more</a></p></body></html>"#;
        let d = super::extract(&html, "https://example.com").unwrap();
        assert_eq!(d.page_type, "article");
        assert!(d.title.contains("Example Domain") || d.markdown.contains("Example Domain"));
        assert!(d.markdown.contains("This domain"));
    }
}
