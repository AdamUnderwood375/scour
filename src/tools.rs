//! API shortcuts: GitHub repo facts and package registry versions.
//!
//! One call → compact markdown, so agents skip scraping JS-rendered pages
//! and bot-walled registries.
#![allow(dead_code)]

use anyhow::{Context, Result, anyhow, bail};
use reqwest::StatusCode;
use serde_json::{Value, json};

use crate::fetcher::client;

const GH_UA: &str = concat!("scour/", env!("CARGO_PKG_VERSION"));
// crates.io rejects generic user agents.
const CRATES_UA: &str = concat!(
    "scour/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/AdamUnderwood375/scour)"
);

/// Compact GitHub repo summary. `spec` is `owner/repo` or any github.com URL.
pub async fn gh(spec: &str) -> Result<String> {
    let (owner, repo) = parse_gh_spec(spec)?;
    let base = format!("https://api.github.com/repos/{owner}/{repo}");
    let auth = gh_auth();
    let headers = gh_headers("application/vnd.github+json", auth.as_deref());

    // Only the repo call is fatal; release lookup failures just drop the line.
    let (repo_res, release) =
        tokio::join!(get(&base, &headers), latest_release(&base, &headers));

    let (st, body) = repo_res?;
    if !st.is_success() {
        return Err(http_err("GitHub", st));
    }
    let repo_json = json_of(&body, &base)?;
    Ok(format_gh(&repo_json, release.as_ref().map(|r| r.as_ref())))
}

/// Cleaned README text (`clean_readme`) for `spec`. Raw media type, so the body is markdown.
pub async fn gh_readme(spec: &str) -> Result<String> {
    let (owner, repo) = parse_gh_spec(spec)?;
    let url = format!("https://api.github.com/repos/{owner}/{repo}/readme");
    let auth = gh_auth();
    let headers = gh_headers("application/vnd.github.raw", auth.as_deref());
    let (st, body) = get(&url, &headers).await?;
    if !st.is_success() {
        return Err(http_err("GitHub", st));
    }
    Ok(clean_readme(&body))
}

fn gh_auth() -> Option<String> {
    std::env::var("GITHUB_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .map(|t| format!("Bearer {t}"))
}

fn gh_headers<'a>(accept: &'a str, auth: Option<&'a str>) -> Vec<(&'a str, &'a str)> {
    let mut h = vec![("User-Agent", GH_UA), ("Accept", accept)];
    if let Some(a) = auth {
        h.push(("Authorization", a));
    }
    h
}

/// Latest release, else newest tag (name only). Outer `None` = lookup failed;
/// `Some(None)` = repo has neither.
async fn latest_release(base: &str, headers: &[(&str, &str)]) -> Option<Option<Value>> {
    let rel_url = format!("{base}/releases/latest");
    let release = get(&rel_url, headers)
        .await
        .ok()
        .filter(|(st, _)| st.is_success())
        .and_then(|(_, body)| json_of(&body, &rel_url).ok());
    if let Some(v) = release {
        return Some(Some(v));
    }
    let tags_url = format!("{base}/tags?per_page=1");
    let (st, body) = get(&tags_url, headers).await.ok()?;
    if !st.is_success() {
        return None;
    }
    let tags = json_of(&body, &tags_url).ok()?;
    Some(
        tags.get(0)
            .and_then(|t| s(t, "name"))
            .map(|n| json!({ "tag_name": n })),
    )
}

/// Latest version of a package. `registry` is crates | npm | pypi (aliases ok).
pub async fn pkg(registry: &str, name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() || name == "." || name == ".." {
        bail!("invalid package name '{name}'");
    }
    let enc_name = enc(name);
    match registry.trim().to_ascii_lowercase().as_str() {
        "crates" | "crate" | "crates.io" => {
            let url = format!("https://crates.io/api/v1/crates/{enc_name}");
            let v = fetch_json("crates.io", &url, &[("User-Agent", CRATES_UA)]).await?;
            Ok(format_crate(&v))
        }
        "npm" | "npmjs" => {
            let url = format!("https://registry.npmjs.org/{enc_name}");
            let v = fetch_json("npm", &url, &[]).await?;
            Ok(format_npm(&v))
        }
        "pypi" | "pip" => {
            let url = format!("https://pypi.org/pypi/{enc_name}/json");
            let v = fetch_json("PyPI", &url, &[]).await?;
            Ok(format_pypi(&v))
        }
        _ => bail!(
            "unknown registry '{registry}' — valid: crates (crate, crates.io), npm (npmjs), pypi (pip)"
        ),
    }
}

// ---------- HTTP helpers ----------

async fn get(url: &str, headers: &[(&str, &str)]) -> Result<(StatusCode, String)> {
    let mut req = client().get(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("request failed: {url}"))?;
    let status = resp.status();
    Ok((status, resp.text().await?))
}

async fn fetch_json(svc: &str, url: &str, headers: &[(&str, &str)]) -> Result<Value> {
    let (status, body) = get(url, headers).await?;
    if !status.is_success() {
        return Err(http_err(svc, status));
    }
    json_of(&body, url)
}

fn json_of(body: &str, url: &str) -> Result<Value> {
    serde_json::from_str(body).with_context(|| format!("invalid JSON from {url}"))
}

// No spec in the message: print_lookups already prefixes it as `error: <spec>: …`.
fn http_err(svc: &str, status: StatusCode) -> anyhow::Error {
    match status.as_u16() {
        404 => anyhow!("not found on {svc}"),
        403 if svc == "GitHub" => {
            anyhow!("GitHub API 403: rate limited — set GITHUB_TOKEN to raise the limit")
        }
        _ => anyhow!("{svc} returned HTTP {status}"),
    }
}

/// Percent-encode a package name for a URL path. `@` and `-._~` stay literal,
/// so npm scopes become `@scope%2Fpkg`.
fn enc(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'@' => {
                char::from(b).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

// ---------- pure helpers (unit-tested) ----------

/// Accepts `owner/repo`, `github.com/o/r`, `https://github.com/o/r/tree/main`,
/// trailing `.git`/`/`. Returns (owner, repo).
pub fn parse_gh_spec(spec: &str) -> Result<(String, String)> {
    let s = spec.trim();
    let path = match s.find("github.com/") {
        Some(i) => &s[i + "github.com/".len()..],
        None => s,
    };
    let path = path.split(['?', '#']).next().unwrap_or("");
    let mut parts = path.split('/').filter(|p| !p.is_empty());
    let owner = parts.next();
    let repo = parts.next().map(|r| r.strip_suffix(".git").unwrap_or(r));
    match (owner, repo) {
        (Some(o), Some(r)) if valid_gh_part(o) && valid_gh_part(r) => {
            Ok((o.to_string(), r.to_string()))
        }
        _ => bail!("expected owner/repo or a github.com URL, got '{spec}'"),
    }
}

fn valid_gh_part(p: &str) -> bool {
    p != "."
        && p != ".."
        && !p.is_empty()
        && p.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Drops badge/image-only lines and HTML comments, collapses blank runs, and trims the ends.
/// Fenced code blocks are kept verbatim (comments and badge-looking lines inside them too).
pub fn clean_readme(raw: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    // Open fence: (marker char, run length).
    let mut fence: Option<(char, usize)> = None;
    let mut in_comment = false;
    for line in raw.lines() {
        let t = line.trim_start();
        let marker = t.chars().next().filter(|c| matches!(c, '`' | '~'));
        let run = marker.map_or(0, |m| t.chars().take_while(|c| *c == m).count());
        if let Some((fc, fl)) = fence {
            if marker == Some(fc) && run >= fl && t[run..].trim().is_empty() {
                fence = None;
            }
            out.push(line.to_string());
            continue;
        }
        if let Some(m) = marker.filter(|_| run >= 3 && !in_comment) {
            fence = Some((m, run));
            out.push(line.to_string());
            continue;
        }
        let line = strip_comments(line, &mut in_comment);
        if is_markup_line(&line) {
            continue;
        }
        if line.trim().is_empty() && out.last().is_none_or(|l| l.trim().is_empty()) {
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out.join("\n")
}

/// Removes `<!-- … -->` spans; a span may cross lines, so `in_comment` carries over.
fn strip_comments(line: &str, in_comment: &mut bool) -> String {
    let mut out = String::new();
    let mut rest = line;
    loop {
        if *in_comment {
            match rest.find("-->") {
                Some(i) => {
                    *in_comment = false;
                    rest = &rest[i + 3..];
                }
                None => return out,
            }
        } else {
            match rest.find("<!--") {
                Some(i) => {
                    out.push_str(&rest[..i]);
                    *in_comment = true;
                    rest = &rest[i + 4..];
                }
                None => {
                    out.push_str(rest);
                    return out;
                }
            }
        }
    }
}

/// True when the line holds only markup: HTML tags, images `![..](..)`, and badge wrappers
/// `[![..](..)](..)`. A line with any readable text (even `[text](url)`) is not markup.
fn is_markup_line(line: &str) -> bool {
    let mut rest = line.trim();
    if rest.is_empty() {
        return false;
    }
    while !rest.is_empty() {
        rest = if rest.starts_with("[!") {
            &rest[1..]
        } else if rest.starts_with('<') {
            match rest.find('>') {
                Some(i) => &rest[i + 1..],
                None => return false,
            }
        } else if rest.starts_with("![") {
            match rest.find("](").and_then(|j| rest[j..].find(')').map(|k| j + k + 1)) {
                Some(e) => &rest[e..],
                None => return false,
            }
        } else if rest.starts_with("](") || rest.starts_with("][") {
            // Wrapper tail: `(href)` inline, or `[ref]` for reference-style badges.
            let close = if rest.starts_with("](") { ')' } else { ']' };
            match rest[2..].find(close) {
                Some(e) => &rest[2 + e + 1..],
                None => return false,
            }
        } else {
            return false;
        };
        rest = rest.trim_start();
    }
    true
}

/// `release`: `None` = lookup failed (line omitted); `Some(None)` = no release or tag.
pub fn format_gh(repo: &Value, release: Option<Option<&Value>>) -> String {
    let mut out = vec![format!("# {}", s(repo, "full_name").unwrap_or("?"))];
    out.extend(s(repo, "description").map(String::from));
    out.push(format!(
        "Stars: {} · Forks: {} · Open issues: {}",
        num(repo, "stargazers_count"),
        num(repo, "forks_count"),
        num(repo, "open_issues_count")
    ));
    let license = s(&repo["license"], "spdx_id")
        .filter(|l| *l != "NOASSERTION")
        .unwrap_or("none");
    out.push(format!(
        "License: {license} · Default branch: {} · Last push: {}",
        s(repo, "default_branch").unwrap_or("?"),
        s(repo, "pushed_at").map_or("?", date)
    ));
    if let Some(release) = release {
        out.push(
            match release.and_then(|r| s(r, "tag_name").map(|t| (t, s(r, "published_at")))) {
                Some((tag, Some(at))) => format!("Latest release: {tag} ({})", date(at)),
                Some((tag, None)) => format!("Latest tag: {tag}"),
                None => "Latest release: none".to_string(),
            },
        );
    }
    if let Some(h) = s(repo, "homepage") {
        out.push(format!("Homepage: {h}"));
    }
    let topics: Vec<&str> = repo["topics"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).take(8).collect())
        .unwrap_or_default();
    if !topics.is_empty() {
        out.push(format!("Topics: {}", topics.join(", ")));
    }
    if let Some(u) = s(repo, "html_url") {
        out.push(format!("URL: {u}"));
    }
    out.join("\n")
}

/// `v` is the crates.io response (`{"crate": {...}}`).
pub fn format_crate(v: &Value) -> String {
    let c = &v["crate"];
    let ver = s(c, "max_stable_version")
        .or(s(c, "max_version"))
        .unwrap_or("?");
    let mut out = vec![format!("# {} {ver}", s(c, "name").unwrap_or("?"))];
    out.extend(s(c, "description").map(String::from));
    out.push(format!(
        "Updated: {} · Downloads: {} total, {} last 90 days",
        s(c, "updated_at").map_or("?", date),
        num(c, "downloads"),
        num(c, "recent_downloads")
    ));
    out.extend(s(c, "name").map(|n| format!("Registry: https://crates.io/crates/{n}")));
    if let Some(r) = s(c, "repository") {
        out.push(format!("Repository: {r}"));
    }
    let docs = s(c, "documentation")
        .map(String::from)
        .or_else(|| s(c, "name").map(|n| format!("https://docs.rs/{n}")));
    out.extend(docs.map(|d| format!("Docs: {d}")));
    out.join("\n")
}

pub fn format_npm(v: &Value) -> String {
    let ver = s(&v["dist-tags"], "latest").unwrap_or("?");
    let license = s(v, "license").or(s(&v["license"], "type")).unwrap_or("?");
    let mut out = vec![format!("# {} {ver}", s(v, "name").unwrap_or("?"))];
    out.extend(s(v, "description").map(String::from));
    out.push(format!(
        "Published: {} · License: {license}",
        s(&v["time"], ver).map_or("?", date)
    ));
    out.extend(s(v, "name").map(|n| format!("Registry: https://www.npmjs.com/package/{n}")));
    let repo = s(v, "repository").or(s(&v["repository"], "url"));
    if let Some(r) = repo {
        let clean = r.strip_prefix("git+").unwrap_or(r);
        let clean = clean.strip_suffix(".git").unwrap_or(clean);
        out.push(format!("Repository: {clean}"));
    }
    if let Some(h) = s(v, "homepage") {
        out.push(format!("Homepage: {h}"));
    }
    out.join("\n")
}

pub fn format_pypi(v: &Value) -> String {
    let info = &v["info"];
    let ver = s(info, "version").unwrap_or("?");
    let mut out = vec![format!("# {} {ver}", s(info, "name").unwrap_or("?"))];
    out.extend(s(info, "summary").map(String::from));
    let uploaded = v["releases"][ver]
        .get(0)
        .and_then(|f| s(f, "upload_time"))
        .map_or("?", date);
    out.push(format!(
        "Released: {uploaded} · Requires Python: {}",
        s(info, "requires_python").unwrap_or("any")
    ));
    out.extend(s(info, "name").map(|n| format!("Registry: https://pypi.org/project/{n}/")));
    if let Some(urls) = info["project_urls"].as_object() {
        let is_pref = |k: &str| {
            let k = k.to_ascii_lowercase();
            ["home", "source", "repo", "code"]
                .iter()
                .any(|p| k.contains(p))
        };
        for (k, u) in urls
            .iter()
            .filter(|(k, u)| u.is_string() && is_pref(k))
            .take(3)
        {
            out.push(format!("{k}: {}", u.as_str().unwrap_or_default()));
        }
    }
    out.join("\n")
}

/// Non-empty string field, or None.
fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v[k].as_str().filter(|x| !x.is_empty())
}

/// Numeric field as text, `?` when missing.
fn num(v: &Value, k: &str) -> String {
    v[k].as_u64()
        .map_or_else(|| "?".to_string(), |n| n.to_string())
}

/// ISO timestamp → date only (`2025-04-02T…` → `2025-04-02`).
fn date(ts: &str) -> &str {
    ts.get(..10).unwrap_or(ts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gh_specs() {
        let ok = |s: &str| parse_gh_spec(s).unwrap();
        let want = ("tokio-rs".to_string(), "axum".to_string());
        assert_eq!(ok("tokio-rs/axum"), want);
        assert_eq!(ok("https://github.com/tokio-rs/axum"), want);
        assert_eq!(ok("https://github.com/tokio-rs/axum/tree/main"), want);
        assert_eq!(ok("github.com/tokio-rs/axum.git"), want);
        assert_eq!(ok("https://github.com/tokio-rs/axum/"), want);
        assert_eq!(ok("  tokio-rs/axum  "), want);
    }

    #[test]
    fn rejects_bad_gh_specs() {
        for bad in [
            "justowner",
            "https://github.com/",
            "a/../b",
            "a/.",
            "a/b c",
            "a/.git",
        ] {
            assert!(parse_gh_spec(bad).is_err(), "should reject {bad}");
        }
    }

    #[test]
    fn formats_gh_with_release() {
        let repo = json!({
            "full_name": "tokio-rs/axum",
            "description": "Ergonomic web framework",
            "stargazers_count": 20000,
            "forks_count": 1500,
            "open_issues_count": 80,
            "license": {"spdx_id": "MIT"},
            "default_branch": "main",
            "pushed_at": "2025-05-01T12:00:00Z",
            "homepage": "https://docs.rs/axum",
            "topics": ["rust", "web", "http", "a", "b", "c", "d", "e", "f"],
            "html_url": "https://github.com/tokio-rs/axum"
        });
        let rel = json!({"tag_name": "axum-v0.8.4", "published_at": "2025-04-02T10:00:00Z"});
        let out = format_gh(&repo, Some(Some(&rel)));
        assert!(out.starts_with("# tokio-rs/axum\nErgonomic web framework\n"));
        assert!(out.contains("Stars: 20000 · Forks: 1500 · Open issues: 80"));
        assert!(out.contains("License: MIT · Default branch: main · Last push: 2025-05-01"));
        assert!(out.contains("Latest release: axum-v0.8.4 (2025-04-02)"));
        assert!(out.contains("Homepage: https://docs.rs/axum"));
        assert!(out.contains("Topics: rust, web, http, a, b, c, d, e\n"));
        assert!(!out.contains(", f"));
        assert!(out.ends_with("URL: https://github.com/tokio-rs/axum"));
        assert!(out.lines().count() <= 12);
    }

    #[test]
    fn formats_gh_tag_fallback_and_nulls() {
        let repo = json!({
            "full_name": "o/r", "description": null, "license": null,
            "stargazers_count": 1, "forks_count": 0, "open_issues_count": 0,
            "default_branch": "master", "pushed_at": "2020-01-01T00:00:00Z",
            "homepage": "", "topics": []
        });
        let tag = json!({"tag_name": "v0.1.0"});
        let out = format_gh(&repo, Some(Some(&tag)));
        assert!(out.contains("License: none"));
        assert!(out.contains("Latest tag: v0.1.0"));
        assert!(!out.contains("Homepage") && !out.contains("Topics") && !out.contains("URL"));
        assert!(format_gh(&repo, Some(None)).contains("Latest release: none"));
        // Failed release+tag lookup: line omitted entirely.
        assert!(!format_gh(&repo, None).contains("Latest"));
    }

    #[test]
    fn http_errors_do_not_repeat_spec() {
        let e = http_err("GitHub", StatusCode::NOT_FOUND).to_string();
        assert_eq!(e, "not found on GitHub");
        let e = http_err("crates.io", StatusCode::BAD_GATEWAY).to_string();
        assert_eq!(e, "crates.io returned HTTP 502 Bad Gateway");
    }

    #[test]
    fn formats_crate() {
        let v = json!({"crate": {
            "name": "axum", "max_stable_version": "0.8.4", "max_version": "0.8.5-rc.1",
            "updated_at": "2025-04-02T10:00:00.123456+00:00",
            "downloads": 50000000, "recent_downloads": 9000000,
            "repository": "https://github.com/tokio-rs/axum",
            "documentation": "https://docs.rs/axum", "description": "Web framework"
        }});
        let out = format_crate(&v);
        assert_eq!(
            out,
            "# axum 0.8.4\nWeb framework\n\
             Updated: 2025-04-02 · Downloads: 50000000 total, 9000000 last 90 days\n\
             Registry: https://crates.io/crates/axum\n\
             Repository: https://github.com/tokio-rs/axum\nDocs: https://docs.rs/axum"
        );
    }

    #[test]
    fn crate_docs_fallback_to_docs_rs() {
        let v = json!({"crate": {"name": "foo-bar", "max_version": "1.0.0", "documentation": null}});
        let out = format_crate(&v);
        assert!(out.contains("Registry: https://crates.io/crates/foo-bar"));
        assert!(out.contains("Docs: https://docs.rs/foo-bar"));
    }

    #[test]
    fn crate_falls_back_to_max_version() {
        let v =
            json!({"crate": {"name": "x", "max_stable_version": null, "max_version": "0.0.1-rc"}});
        assert!(format_crate(&v).starts_with("# x 0.0.1-rc"));
    }

    #[test]
    fn formats_npm() {
        let v = json!({
            "name": "react", "description": "React is a JavaScript library",
            "dist-tags": {"latest": "19.1.0"},
            "time": {"19.1.0": "2025-03-28T15:00:00.000Z"},
            "license": "MIT",
            "repository": {"type": "git", "url": "git+https://github.com/facebook/react.git"},
            "homepage": "https://react.dev/"
        });
        assert_eq!(
            format_npm(&v),
            "# react 19.1.0\nReact is a JavaScript library\n\
             Published: 2025-03-28 · License: MIT\n\
             Registry: https://www.npmjs.com/package/react\n\
             Repository: https://github.com/facebook/react\nHomepage: https://react.dev/"
        );
    }

    #[test]
    fn formats_pypi() {
        let v = json!({
            "info": {
                "name": "requests", "version": "2.32.3", "summary": "Python HTTP for Humans.",
                "requires_python": ">=3.8",
                "project_urls": {
                    "Changelog": "https://example.com/c",
                    "Documentation": "https://requests.readthedocs.io",
                    "Homepage": "https://requests.readthedocs.io",
                    "Source": "https://github.com/psf/requests"
                }
            },
            "releases": {"2.32.3": [{"upload_time": "2024-05-29T15:37:47", "upload_time_iso_8601": "x"}]}
        });
        let out = format_pypi(&v);
        assert!(out.starts_with("# requests 2.32.3\nPython HTTP for Humans.\n"));
        assert!(out.contains("Released: 2024-05-29 · Requires Python: >=3.8"));
        assert!(out.contains("Registry: https://pypi.org/project/requests/"));
        assert!(out.contains("Homepage: https://requests.readthedocs.io"));
        assert!(out.contains("Source: https://github.com/psf/requests"));
        assert!(!out.contains("Changelog") && !out.contains("Documentation:"));
        assert!(out.lines().count() <= 10);
    }

    #[test]
    fn encodes_npm_scope() {
        assert_eq!(enc("@scope/pkg"), "@scope%2Fpkg");
        assert_eq!(enc("axum"), "axum");
    }

    #[tokio::test]
    async fn unknown_registry_errors() {
        let err = pkg("maven", "x").await.unwrap_err().to_string();
        assert!(err.contains("unknown registry 'maven'"), "{err}");
        assert!(err.contains("crates") && err.contains("npm") && err.contains("pypi"));
    }

    #[test]
    fn readme_cleaning_drops_badges_and_comments_keeps_code() {
        let raw = "<p align=\"center\">\n<img src=\"logo.png\">\n</p>\n\n# Axum\n[![CI](https://x/ci.svg)](https://x/ci) [![MIT](https://x/m.svg)](LICENSE)\n[Getting started](docs/start.md)\n\n<!-- TODO\nhidden\n-->\n\n\n\nText with [docs](https://d) and <a href=\"x\">link</a>.\n\n```rust\n![not](badge)\n<!-- keep -->\n\n\nlet x = Vec::<u8>::new();\n```\n";
        assert_eq!(
            clean_readme(raw),
            "# Axum\n[Getting started](docs/start.md)\n\nText with [docs](https://d) and <a href=\"x\">link</a>.\n\n```rust\n![not](badge)\n<!-- keep -->\n\n\nlet x = Vec::<u8>::new();\n```"
        );
    }

    #[test]
    fn readme_cleaning_handles_inline_comment_and_empty() {
        assert_eq!(clean_readme("a <!-- x --> b\n<!--\n\n-->\nc\n"), "a  b\n\nc");
        assert_eq!(clean_readme("\n\n  \n"), "");
        assert_eq!(clean_readme("[![Docs](https://d/b.svg)][docs]\n\nkeep"), "keep");
    }

    #[tokio::test]
    #[ignore]
    async fn live_gh() {
        let out = gh("tokio-rs/axum").await.unwrap();
        println!("{out}");
    }

    #[tokio::test]
    #[ignore]
    async fn live_crates() {
        println!("{}", pkg("crates", "axum").await.unwrap());
    }

    #[tokio::test]
    #[ignore]
    async fn live_npm() {
        println!("{}", pkg("npm", "react").await.unwrap());
    }

    #[tokio::test]
    #[ignore]
    async fn live_pypi() {
        println!("{}", pkg("pypi", "requests").await.unwrap());
    }
}
