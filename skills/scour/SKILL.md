---
name: scour
description: Fast local web scraper + keyless search (Rust CLI, no MCP tax). Search+fetch in one call, GitHub/package facts, batch, crawl, links, screenshots. Returns clean, size-capped markdown. Prefer this over any web_fetch/web_search MCP.
---

# Scour: web research from bash

Bin: `S=$(command -v scour || echo ~/scour/target/release/scour)` (`$S --help` for all flags).

Wall clock and tokens are dominated by **model turns**, not fetching. Every recipe below is ONE bash call. Aim for 1–3 scour calls per research task.

## Pick the recipe

| Need | One call |
|---|---|
| Stars, forks, license, latest release of a GitHub repo | `$S gh tokio-rs/axum actix/actix-web` |
| ...plus what it is / its features | `$S gh tokio-rs/axum actix/actix-web --readme --max-chars 800` |
| Latest version of a package | `$S pkg crates axum actix-web` (also `npm`, `pypi`) |
| Articles / discussions / "what's the consensus" | `$S search "axum vs actix web 2025" --max 8 --fetch 3` |
| Docs pages you know | `printf '%s\n' URL1 URL2 \| $S batch --focus "middleware from_fn" --max-chars 3000` |
| Find sub-pages of a docs site | `$S links https://docs.rs/axum/latest/axum/ --filter middleware --max 15` |
| Read a docs section in depth / need code examples | `$S crawl https://docs.rs/axum/latest/axum/middleware/ --max-pages 4 --focus "from_fn" --max-chars 5000` |
| One page | `$S 'https://example.com/page' --max-chars 3000 focus words` |
| See a page | `$S screenshot URL -o /tmp/x.png`, then read the PNG |

Combine facts and articles in the same bash call:
```bash
S=$(command -v scour || echo ~/scour/target/release/scour)
$S gh tokio-rs/axum actix/actix-web --readme --max-chars 800; $S pkg crates axum actix-web
$S search "axum vs actix web comparison 2025" --max 8 --fetch 3 --max-chars 2500
```

## Rules (each one measurably saves turns)

1. **Facts (versions, stars, release dates, features): use `gh`/`pkg` (+ `--readme`), never scrape HTML.** GitHub star counts are JS-rendered, lib.rs serves bot walls, and crates.io needs a slow headless-browser fetch.
2. **Use `search --fetch N` instead of search, then fetch.** It fetches the top N hits with the query as focus at 2500 chars each. Hits come from several engines (Bing/Yahoo/Brave/DDG/Mojeek), are fused, and pages covering *all* your terms rank first. The engines are paced, and blocked ones cool down automatically.
3. **Two searches max.** If the first misses, rephrase once (add a site, a year, or "vs"/"comparison"). Then work with what you have. `--site reddit.com` and `--freshness year` narrow results. A `<!-- warning: low relevance … -->` line means the engines returned off-topic hits (usually throttling). Rephrase with fewer, more distinctive terms instead of firing more searches.
4. **Never loop `scour` per URL.** Pipe the URLs into `batch`: one process, parallel, input order.
5. **Single-quote URLs.** A bare `&` backgrounds the shell command and truncates the URL.
6. **Don't re-fetch a page already in context.** If a page was truncated and you need the rest, paste the command from its `<!-- truncated … more: scour scrape … -->` hint. It continues exactly where the output stopped.
7. **Research ends at the report.** Do not create projects, `cargo build`, or run servers to "verify" an example unless the task explicitly says to compile/run it. Docs examples are already verified.
8. **Write the answer file with the `write` tool** right after the last fetch. Don't re-read it.

## Output

- stdout is markdown. `scrape` prints `# <title>`, a `<!-- tier … content_ok … -->` line, then content. `batch`, `crawl` and `search --fetch` print each page as `## <url>` plus content. `search` prints `N. title — url` plus a snippet line. Failed pages print `<!-- error: … -->`.
- Default `--max-chars` is 6000 for scrape/batch and 2500 for `--fetch`/crawl pages. Code examples can get cut off at those sizes; raise it when you need code.
- `focus` (positional words for scrape, `--focus` everywhere) keeps the most relevant paragraphs (IDF-weighted, headings boosted). Use it whenever you know what you need. `<!-- focus matched nothing … -->` means you got the whole page.
- `content_ok: false` means a bot wall or empty shell. Use another source, don't retry.
- `--json` (scrape/batch/search/crawl) gives structured output; search includes per-engine status. `--stats` prints `stats: {json}` lines on stderr with `est_tokens`, `cache`, `tier`, `truncated`, and for search `engines`, e.g. `["ddg:ok:10","bing:ok:9","brave:blocked:http429"]`. If every engine is blocked, search errors and lists their statuses. Don't hammer it; use `gh`/`pkg`/known URLs instead.
- Exit codes: 2 = usage error, 1 = failure (for `gh`/`pkg`: any item failed; for `batch`/`--fetch`: all pages failed). Errors go to stderr.

## Notes

- Tiers: HTTP with a Chrome UA first. It escalates to headless Chromium on a block (401/403/429/503, tiny body, Cloudflare). Set `SCOUR_BROWSER=/path` to choose the browser.
- Private/loopback/metadata addresses are blocked, including via DNS or redirects. scour cannot fetch localhost.
- PDFs (by URL or by `%PDF` content) are converted to text via `pdftotext`. Cache: 1h at `~/.cache/scour/cache.json`. Use `$S cache-clear` for freshness-sensitive runs.
- `gh` uses `$GITHUB_TOKEN` if set (unauthenticated: 60 req/h; `--readme` costs 1 extra request per repo).
- Keep `~/.pi/agent/extensions/scour.ts.off` off. The MCP form costs ~1.2k schema tokens and 40k-char results that get re-sent every turn.
