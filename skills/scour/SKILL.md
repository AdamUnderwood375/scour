---
name: scour
description: Fast local web scraper (Rust). CLI runner for web scraping — fetches clean markdown via `~/scour/target/release/scour`. Use for single pages, batch (parallel), crawl, and keyless search. Handles articles + lists, BM25 focus, 1hr cache. Prefer this over web_fetch MCP.
---

# Scour — local scraper via CLI

No MCP tax. Run via `~/scour/target/release/scour` (7ms startup). Call via `bash`, parse output directly to save context tokens.

## When to use
- Fetch a URL, search the web, crawl docs, or extract clean markdown.
- Use instead of `web_fetch`/`web_search`/`web_crawl` MCP tools to avoid token bloat.

## CLI

Bin: `~/scour/target/release/scour` (override browser path with `SCOUR_BROWSER`).

### 1. Single page (most common)
```bash
~/scour/target/release/scour "https://example.com" 2>&1 | head -n 200
~/scour/target/release/scour "https://example.com" "focus query" 2>&1 | head -n 200
```
Focus = BM25 + heading boost, returns only relevant paragraphs.
- `# Title` on first line, followed by clean markdown
- stderr: `page_type: article|list quality: 0.70 links: N next_offset: ...`

### 2. JSON via MCP stdio (when structured output is required)
```bash
python3 - << 'PY'
import subprocess, json
p = subprocess.Popen(["/home/adam-underwood/scour/target/release/scour"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1)
p.stdin.write(json.dumps({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}})+"\n"); p.stdin.flush()
p.stdout.readline()
p.stdin.write('{"jsonrpc":"2.0","method":"notifications/initialized"}\n'); p.stdin.flush()
p.stdin.write(json.dumps({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"scrape","arguments":{"url":"https://example.com"}}})+"\n"); p.stdin.flush()
print(p.stdout.readline()[:2000])
p.terminate()
PY
```
Tools: `scrape`, `scrape_batch`, `crawl`, `search`, `cache_clear`, `scour_version`.

### 3. Batch (parallel)
```bash
printf "%s\n" "https://example.com" "https://react.dev/learn" | xargs -P3 -I{} ~/scour/target/release/scour "{}" 2>&1 | head -n 300
```

### 4. Crawl docs
Run via MCP `crawl` tool (url, max_pages=5, max_depth=2, focus).

### 5. Search
Run via MCP `search` tool (keyless DDG→Bing fallback: query, max_results).

## Token Savings
Calling `scour` via CLI/bash saves 2k–3k tokens per session compared to registering full MCP tool schemas in the model prompt.
