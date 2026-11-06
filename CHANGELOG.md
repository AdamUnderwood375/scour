# Changelog

All notable changes to this project are documented here. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.2.0] - 2026-11-06

### Added
- `search --fetch N` fetches the top hits in the same call, with `--focus`, `--max-chars`, and `--json`.
- `gh` (GitHub stars, license, latest release; `--readme` excerpt) and `pkg` (latest crates.io, npm, PyPI versions). Matching MCP tools: `github_repo`, `package_info`.
- `batch`, `links`, `crawl` (best-first, focus-aware, `--sitemap`), and `screenshot` commands.
- `--offset` and paste-able continuation hints on truncated output; `--json` and `--stats` on the main commands.
- Search engines: Brave, Yahoo, and Mojeek, plus a DuckDuckGo lite fallback. Results are fused with reciprocal-rank fusion, with `site` and `freshness` filters, per-engine status, and a low-relevance warning.
- PDF text extraction via `pdftotext`, with focus support.
- Playwright-managed Chromium as a browser-tier fallback.
- macOS browser auto-detection (Brave, Google Chrome, Chromium, Playwright cache).
- Main-content root selection, boilerplate stripping, and verbatim code blocks in extraction.
- Wikipedia infoboxes render after the lead paragraph.
- Extraction caps nesting depth.
- Size-aware class filtering for boilerplate removal.
- Prebuilt release binaries for Linux and macOS (x86_64 and aarch64).
- `bench/`: reproducible benchmark harness.
- `--version` prints JSON (version, tools, features).

### Changed
- `main.rs` split into `cli.rs`, `mcp.rs`, and `scrape.rs`.
- Fetch timeouts: 6 s connect, 15 s total, 25 s per-page deadline in `batch` and `--fetch`; 20 MB body cap.
- Focus is fence-aware and IDF-weighted, with heading boosts. Truncation cuts on paragraph boundaries.
- Search pacing is shared across processes. Blocked engines cool down (10 min DuckDuckGo, 2 min others); cooldowns are bypassed when every engine is cooling.
- Failed fetches are no longer cached.
- MCP tool descriptions rewritten to match behavior.
- Dependencies updated (scraper 0.29, rmcp 3.9, lru 0.19, base64 0.23, tokio, reqwest, ammonia). CI checks fmt, clippy, tests on Linux and macOS, and MSRV 1.89; Dependabot enabled.

### Fixed
- Timeouts no longer escalate to the headless browser.
- Multi-byte text is never split during slicing or truncation.
- Bing redirect wrappers and DuckDuckGo root-relative links decode correctly.
- Markdown junk removal no longer touches fenced code.
- `page_type` is reported correctly.
- DNS failures show a clearer error message.

### Security
- SSRF hardening: embedded IPv4 in NAT64, 6to4, and IPv4-mapped addresses is judged by the IPv4 address.
- The HTTP resolver drops non-public addresses before connecting, including on redirect hops. Redirects are capped at 10 hops.
- Browser tier checks the URL before navigation and the final URL after redirects, including for `actions` and `screenshot`.
- Cache keys include an action hash, so action results never leak across requests.

## [0.1.0] - 2026-09-08

- Initial release: HTTP-first scraping with headless-Chromium fallback, markdown output with focus and size caps, search, and an MCP stdio server.

[Unreleased]: https://github.com/AdamUnderwood375/scour/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/AdamUnderwood375/scour/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/AdamUnderwood375/scour/tree/v0.1.0
