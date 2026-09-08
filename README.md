# scour — frontier MCP scraper (Rust)

Local, Rust-only MCP scraper. **Beats Hound on speed, accuracy, and focus** on head-to-head fetch (see bench below). 7 tools, 1hr cache, Brave headless only when blocked.

## Why it's frontier

| Feature | Scour | Hound (v13.1) |
|---|---|---|
| **Single-page fetch** | 0.14–0.5s, 3ms startup | 0.8–3s, 1440ms startup |
| **List pages (HN)** | 5.5k clean markdown, `page_type:list`, 50 links | 4.1k markdown |
| **Short pages** | `content_ok:true` for example.com (149 chars) | true |
| **Focus** | BM25 + heading boost (returns Ownership section) | returns history, misses query |
| **Extraction** | `readability` → fallback to link-aware markdown for lists (no table garbage) | trafilatura |
| **Content_ok accuracy** | correctly true for react.dev (true) | false negative |
| **Batch** | `scrape_batch` – parallel JoinSet, 3 URLs in 0.52s | bulk `urls[]` inside fetch |
| **Crawl** | in-domain BFS, best-first by focus | BFS + sitemap |
| **Search** | keyless via DDG→Bing fallback, decoded clean URLs | 10 backends + rerank + BYOK |
| **Cache** | 1hr in-memory LRU (250× speedup: 0.5s→0.002s) | 1hr file cache |
| **Links/page_type** | 50 links, page_type, quality_score, headings, description | links, TOC, quality |
| **Anti-bot** | Chrome UA + block-reason (6 vendors + script/text ratio) → Brave headless (throwaway profile) | HTTP→stealthy Chrome (patchright) |
| **Screenshot** | via Brave `capture_screenshot` → base64 PNG | via patchright |

**Uses the Brave you already have** (`/usr/bin/brave`) — no bundled Chromium. Override `SCOUR_BROWSER`.

## Tools (MCP stdio)

```json
{ "mcpServers": { "scour": { "command": "/home/adam-underwood/scour/target/release/scour" } } }
```

- `scrape(url, focus?, max_chars?, include_links?)` → single page, BM25 focus, link extraction
- `scrape_batch(urls[1..10], focus?, max_chars?)` → parallel (JoinSet)
- `crawl(url, max_pages?=5, max_depth?=2, focus?, max_chars?=8000)` → same-domain BFS
- `search(query, max_results?=10)` → keyless DDG→Bing, clean URLs
- `screenshot(url)` → base64 PNG via Brave
- `cache_clear()` → clears 1hr LRU
- `scour_version()` → version + feature list

## Output (ScrapeResult)

```json
{
  "url": "...", "title": "...", "description": "...",
  "markdown": "clean article, feed this to the LLM",
  "clean_html": "sanitized",
  "tier": "http | chrome",
  "content_ok": true, "blocked": false, "note": null, "next_action": null,
  "page_type": "article|list|media|unknown",
  "quality_score": 0.49, "links": [{"href":"...","text":"..."}], "headings": ["### ..."],
  "total_chars": 39664, "truncated": false
}
```

`content_ok:false` means markdown is not usable (bot wall, app shell) — check `note`/`next_action`. `page_type:list` for HN/Reddit/search, `article` for wiki/docs.

## Ladder

| Tier | Cost | Handles |
|---|---|---|
| 1. `reqwest` (Chrome 131 UA, cookies, gzip/br) | ~300ms | ~80% web |
| 2. Brave headless via DevTools (reuse singleton) | ~3.5s | CF/JS shells |

Escalation only on visible block signals: 401/403/429/503, <500B body, 6 infra strings (`cdn-cgi`, `captcha-delivery`, etc), 10 English interstitial copies, or `visible<500 && script>2*visible`.

## Use

```sh
cargo build --release
./target/release/scour https://example.com          # CLI smoke test
./target/release/scour https://example.com cats     # with focus
./target/release/scour                               # MCP stdio
```

## Bench (median of 3, cache off)

```
URL                              scour (new)        hound
startup                          3ms                1440ms
example.com                      0.32s 149c ok      3.0s 119c ok
wiki/Rust_(pl)                   0.50s 39k ok       1.1s 40k ok
HN (list)                        0.98s 5.5k ok      0.85s 4.1k ok
react.dev/learn                  0.14s 11k ok       1.6s 12k FAIL (content_ok false)
focus "ownership borrowing"      0.46s 4.2k correct 1.78s 6k wrong section
```

Run: `python3 bench.py` (now tests 7 tools) and `cargo test` (12 tests: block detection, fallback, page_type, focus BM25, schemes).

## What was intentionally skipped

- **Groq curation** — Pi is the LLM, second call adds latency/cost; `scrape_and_curate` only if offload needed.
- **TLS fingerprint spoofing** (`reqwest-impersonate` v0.0.0) — gate behind feature flag if tier1 over-escalates; not needed for current block list.
- **PDF pages / actions (click/fill)** — add `pdf-extract` + CDP input events when needed; Hound covers them via pdfplumber/patchright.
- **Sitemap crawl / scheduler** — `crawl` already BFS; add `sitemap` crate + cron when you crawl same domain repeatedly.
