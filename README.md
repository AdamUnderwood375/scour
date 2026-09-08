# scour — fast local web scraper & search (Rust)

Local, Rust-only web scraper and search engine. Fetches clean markdown for single pages, batch scraping, BFS site crawl, keyless web search, BM25 focus, 1-hour cache, and Brave headless fallback when blocked.

## Core Features

- **Single-page extraction**: Fast fetches with instant in-memory and persistent file cache.
- **List & article awareness**: Extracts clean markdown with `page_type` detection (`article`, `list`, `media`), quality scoring, and link extraction without table garbage.
- **BM25 Focus**: Query-driven relevance weighting + heading boost to pull specific sections from large documents.
- **Batch scraping**: Parallel processing via `scrape_batch` (JoinSet).
- **In-domain crawl**: Same-domain BFS crawl prioritized by focus query.
- **Keyless search**: Parallel DuckDuckGo HTML + Bing fallback with clean, decoded URLs.
- **Cache**: 1-hour in-memory LRU + file cache (`~/.cache/scour/cache.json`).
- **Anti-bot ladder**: Standard HTTP with Chrome UA; escalates to Brave headless (`/usr/bin/brave`) only on challenge detection (Cloudflare, interstitial blocks, JS app shells).
- **Screenshot**: Headless screenshot capture returning base64 PNG.

---

## Agent Skill Option (Save Tokens vs MCP)

Running scrapers as MCP tools injects large tool schemas and documentation into the model's context window on every turn (~2,000–3,000 tokens overhead per session).

With the **Pi Agent Skill**, your coding agent runs `scour` directly through standard CLI execution instead of registering an MCP server:
- **0 MCP schema token tax** — saves ~2k–3k context tokens per session.
- **Direct stdout pipes** — agents pipe output directly into `head`, `grep`, or file targets.
- **Zero background daemon** — executes on demand and exits immediately.

### Installing the Skill

Copy `skills/scour/SKILL.md` to your Pi agent skills directory:

```bash
mkdir -p ~/.pi/agent/skills/scour
cp skills/scour/SKILL.md ~/.pi/agent/skills/scour/
```

### How the Agent Uses It

The agent uses its existing `bash` tool to invoke `scour`:

```bash
# Fetch clean markdown
~/scour/target/release/scour "https://example.com" 2>&1 | head -n 200

# Scrape with BM25 focus query
~/scour/target/release/scour "https://react.dev/learn" "state" 2>&1 | head -n 200

# Parallel batch fetch
printf "%s\n" "https://example.com" "https://react.dev/learn" | xargs -P3 -I{} ~/scour/target/release/scour "{}" 2>&1
```

The CLI prints the title on the first line, clean markdown on stdout, and metadata (`page_type`, `quality`, `links`) on stderr.

---

## Escalation Ladder

| Tier | Latency | Scope |
|---|---|---|
| 1. `reqwest` (Chrome 131 UA, cookies, gzip/br) | ~300ms | Handles the majority of standard web pages |
| 2. Brave headless via DevTools (reuse singleton) | ~3.5s | Cloudflare, JS app shells, anti-bot interstitials |

Escalation occurs only on explicit block signals: `401`/`403`/`429`/`503`, `<500B` body, challenge signatures (`cdn-cgi`, `captcha-delivery`), or low visible text with a high script ratio.

---

## Building & CLI Use

```sh
cargo build --release

# CLI smoke test
./target/release/scour https://example.com

# CLI with BM25 focus query
./target/release/scour https://example.com "query"

# Run as stdio MCP server
./target/release/scour
```

---

## MCP Tools (stdio)

If you prefer structured MCP integration, add `scour` to your MCP configuration:

```json
{
  "mcpServers": {
    "scour": {
      "command": "/home/adam-underwood/scour/target/release/scour"
    }
  }
}
```

### Available Tools

- `scrape(url, focus?, max_chars?, include_links?, offset?, options?)` — Scrapes a single page.
- `scrape_batch(urls[1..10], focus?, max_chars?)` — Scrapes up to 10 URLs in parallel.
- `crawl(url, max_pages?=5, max_depth?=2, focus?, max_chars?=8000)` — In-domain BFS crawl.
- `search(query, max_results?=10)` — Keyless search across DDG and Bing.
- `screenshot(url)` — Captures a full-page screenshot via Brave headless.
- `cache_clear()` — Flushes the 1-hour cache.
- `scour_version()` — Returns binary version and active feature set.

### Output Schema (`ScrapeResult`)

```json
{
  "url": "https://example.com",
  "title": "Example Domain",
  "description": null,
  "markdown": "This domain is for use in illustrative examples in documents...",
  "clean_html": "<div>...</div>",
  "tier": "http",
  "content_ok": true,
  "blocked": false,
  "note": null,
  "next_action": null,
  "page_type": "article",
  "quality_score": 0.85,
  "links": [{"href": "https://www.iana.org/domains/example", "text": "More information..."}],
  "headings": ["# Example Domain"],
  "total_chars": 149,
  "truncated": false
}
```
