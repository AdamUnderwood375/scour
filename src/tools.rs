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
    let what = format!("{owner}/{repo}");
    let base = format!("https://api.github.com/repos/{what}");
    let auth = std::env::var("GITHUB_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .map(|t| format!("Bearer {t}"));
    let mut headers = vec![
        ("User-Agent", GH_UA),
        ("Accept", "application/vnd.github+json"),
    ];
    if let Some(a) = &auth {
        headers.push(("Authorization", a.as_str()));
    }

    let rel_url = format!("{base}/releases/latest");
    let (repo_res, rel_res) = tokio::join!(get(&base, &headers), get(&rel_url, &headers));

    let (st, body) = repo_res?;
    if !st.is_success() {
        return Err(http_err("GitHub", st, &what));
    }
    let repo_json = json_of(&body, &base)?;

    let (rst, rbody) = rel_res?;
    let release = if rst.is_success() {
        Some(json_of(&rbody, &rel_url)?)
    } else if rst == StatusCode::NOT_FOUND {
        // No releases: fall back to the newest tag (name only, no date).
        let tags_url = format!("{base}/tags?per_page=1");
        let (tst, tbody) = get(&tags_url, &headers).await?;
        if !tst.is_success() {
            return Err(http_err("GitHub", tst, &what));
        }
        json_of(&tbody, &tags_url)?
            .get(0)
            .and_then(|t| s(t, "name"))
            .map(|n| json!({ "tag_name": n }))
    } else {
        return Err(http_err("GitHub", rst, &what));
    };

    Ok(format_gh(&repo_json, release.as_ref()))
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
            let v = fetch_json("crates.io", name, &url, &[("User-Agent", CRATES_UA)]).await?;
            Ok(format_crate(&v))
        }
        "npm" | "npmjs" => {
            let url = format!("https://registry.npmjs.org/{enc_name}");
            let v = fetch_json("npm", name, &url, &[]).await?;
            Ok(format_npm(&v))
        }
        "pypi" | "pip" => {
            let url = format!("https://pypi.org/pypi/{enc_name}/json");
            let v = fetch_json("PyPI", name, &url, &[]).await?;
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

async fn fetch_json(svc: &str, what: &str, url: &str, headers: &[(&str, &str)]) -> Result<Value> {
    let (status, body) = get(url, headers).await?;
    if !status.is_success() {
        return Err(http_err(svc, status, what));
    }
    json_of(&body, url)
}

fn json_of(body: &str, url: &str) -> Result<Value> {
    serde_json::from_str(body).with_context(|| format!("invalid JSON from {url}"))
}

fn http_err(svc: &str, status: StatusCode, what: &str) -> anyhow::Error {
    match status.as_u16() {
        404 => anyhow!("{what} not found on {svc}"),
        403 if svc == "GitHub" => {
            anyhow!("GitHub API 403 for {what}: rate limited — set GITHUB_TOKEN to raise the limit")
        }
        _ => anyhow!("{svc} returned HTTP {status} for {what}"),
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

pub fn format_gh(repo: &Value, release: Option<&Value>) -> String {
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
    out.push(
        match release.and_then(|r| s(r, "tag_name").map(|t| (t, s(r, "published_at")))) {
            Some((tag, Some(at))) => format!("Latest release: {tag} ({})", date(at)),
            Some((tag, None)) => format!("Latest tag: {tag}"),
            None => "Latest release: none".to_string(),
        },
    );
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
    if let Some(r) = s(c, "repository") {
        out.push(format!("Repository: {r}"));
    }
    if let Some(d) = s(c, "documentation") {
        out.push(format!("Docs: {d}"));
    }
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
        let out = format_gh(&repo, Some(&rel));
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
        let out = format_gh(&repo, Some(&tag));
        assert!(out.contains("License: none"));
        assert!(out.contains("Latest tag: v0.1.0"));
        assert!(!out.contains("Homepage") && !out.contains("Topics") && !out.contains("URL"));
        assert!(format_gh(&repo, None).contains("Latest release: none"));
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
             Repository: https://github.com/tokio-rs/axum\nDocs: https://docs.rs/axum"
        );
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
