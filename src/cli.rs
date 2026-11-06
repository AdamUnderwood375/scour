//! The CLI: usage text, argument parsing, option structs, command handlers,
//! output helpers, and parallel fetch.

use crate::scrape::{
    Stats, crawl_run, cut_page, focus_filter, page_links, run_ext_stats, screenshot_png, validate,
};
use crate::search::search_run;
use crate::types::*;
use crate::{extractor, fetcher, tools};
use std::collections::HashSet;
use std::time::Duration;

// ── CLI ─────────────────────────────────────────────────────────────────────

pub(crate) const USAGE: &str = "\
scour - local web scraper for agents (HTTP first, headless Chrome when blocked)

usage:
  scour <url> [focus...] [flags]          scrape one page (same as `scrape`)
  scour scrape <url> [focus...] [--focus Q] [--max-chars N] [--offset N] [--json] [--stats]
  scour batch [urls...] [--focus Q] [--max-chars N] [--parallel N] [--json] [--stats]
                                          urls from stdin if none (# lines are comments)
  scour search <query...> [--max N] [--site D] [--freshness day|week|month|year]
               [--fetch N] [--focus Q] [--max-chars N] [--json] [--stats]
                                          --fetch N scrapes the top N hits (max 8)
  scour gh <owner/repo|github-url>... [--readme] [--max-chars N] [--focus Q]
                                          GitHub repo: stars, license, latest release;
                                          --readme adds a README excerpt (1500 chars)
  scour pkg <crates|npm|pypi> <name>...   latest version + description
  scour links <url> [--filter S] [--same-site] [--max N]
  scour crawl <url> [--max-pages N] [--depth N] [--focus Q] [--max-chars N] [--sitemap] [--json] [--stats]
  scour screenshot <url> [-o PATH]        PNG (default /tmp/scour-<unix_ts>.png)
  scour cache-clear
  scour --version | -V | --help | -h
  scour                                   MCP stdio server

defaults: --max-chars 6000 (scrape, batch), 2500 (search --fetch, crawl per page);
          --max-pages 10 (max 50); --depth 2; --max 10 (search), 100 (links)
";

/// `--help`, `-h` or `help` as the first token.
pub(crate) fn is_help(arg: Option<&str>) -> bool {
    matches!(arg, Some("--help" | "-h" | "help"))
}

/// Usage error: message and usage on stderr, exit 2.
pub(crate) fn usage(msg: &str) -> ! {
    eprintln!("error: {msg}\n\n{USAGE}");
    std::process::exit(2)
}

/// Inputs accepted by batch, gh and pkg; extras are dropped with a warning.
const MAX_INPUTS: usize = 20;

fn cap_inputs(mut items: Vec<String>, what: &str) -> Vec<String> {
    if items.len() > MAX_INPUTS {
        eprintln!(
            "warning: {what}: {} given, first {MAX_INPUTS} processed; dropped {}",
            items.len(),
            items.len() - MAX_INPUTS
        );
        items.truncate(MAX_INPUTS);
    }
    items
}

/// A first word that is neither a URL nor a flag: a mistyped subcommand.
pub(crate) fn is_bare_word(s: &str) -> bool {
    !s.starts_with('-') && !s.contains('.') && !s.contains("://")
}

/// Single-quoted shell word.
fn sh_single(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Markdown body plus, when truncated, a paste-able command for the rest. The command repeats
/// the focus and max-chars that produced `next_offset`, so the offset lines up with the same text.
fn with_hint(
    markdown: &str,
    url: &str,
    next_offset: Option<usize>,
    focused_chars: usize,
    focus: Option<&str>,
    max_chars: usize,
) -> String {
    let Some(n) = next_offset else {
        return markdown.to_string();
    };
    let mut cmd = format!(
        "scour scrape {} --offset {n} --max-chars {max_chars}",
        sh_single(url)
    );
    if let Some(f) = focus {
        cmd += &format!(" --focus {}", sh_single(f));
    }
    // focused_chars is 0 for results cached before the field existed.
    let of = if focused_chars > 0 {
        format!(" of {focused_chars}")
    } else {
        String::new()
    };
    format!("{markdown}\n\n<!-- truncated at {n}{of} chars; more: {cmd} -->")
}

// Shared markdown emitter for the shim and the `scrape` subcommand.
fn emit_scrape(r: &ScrapeResult, focus: Option<&str>, max_chars: usize) {
    if let Some(n) = &r.note {
        eprintln!("warning: {n}");
    }
    if let Some(a) = &r.next_action {
        eprintln!("next: {a}");
    }
    eprintln!(
        "page_type: {} quality: {:.2} links: {} headings: {} next_offset: {:?} media: {}",
        r.page_type,
        r.quality_score,
        r.links.len(),
        r.headings.len(),
        r.next_offset,
        r.media.len()
    );
    println!(
        "# {}\n<!-- tier: {} content_ok: {} chars: {} -->\n\n{}",
        r.title,
        r.tier,
        r.content_ok,
        r.total_chars,
        with_hint(
            &r.markdown,
            &r.url,
            r.next_offset,
            r.focused_chars,
            focus,
            max_chars
        )
    );
}

/// Placeholder result for a URL whose fetch failed (MCP batch).
pub(crate) fn failed_doc(url: &str, note: String) -> ScrapeResult {
    ScrapeResult {
        url: url.to_string(),
        title: String::new(),
        markdown: String::new(),
        clean_html: String::new(),
        tier: "http".to_string(),
        content_ok: false,
        blocked: false,
        note: Some(note),
        next_action: Some("try another URL or check connectivity".to_string()),
        total_chars: 0,
        truncated: false,
        page_type: "unknown".to_string(),
        quality_score: 0.0,
        description: None,
        links: vec![],
        headings: vec![],
        next_offset: None,
        media: vec![],
        focused_chars: 0,
    }
}

/// Value of the flag at `args[*i]`; leaves `*i` on the value.
fn flag_value<'a>(args: &'a [String], i: &mut usize) -> Result<&'a str, String> {
    *i += 1;
    match args.get(*i) {
        Some(v) => Ok(v.as_str()),
        None => Err(format!("{} needs a value", args[*i - 1])),
    }
}

fn parse_num<T: std::str::FromStr>(s: &str, flag: &str) -> Result<T, String> {
    s.parse()
        .map_err(|_| format!("{flag} needs a number, got '{s}'"))
}

/// Like `parse_num`, for counts that must be at least 1 (`--max-chars 0` would never advance).
fn parse_pos(s: &str, flag: &str) -> Result<usize, String> {
    match parse_num::<usize>(s, flag)? {
        0 => Err(format!("{flag} must be at least 1")),
        n => Ok(n),
    }
}

/// Non-flag args for subcommands without flags beyond the list itself.
fn positionals(args: &[String]) -> Result<Vec<String>, String> {
    if let Some(f) = args.iter().find(|a| a.starts_with('-')) {
        return Err(format!("unknown flag {f}"));
    }
    Ok(args.to_vec())
}

fn one_url(mut words: Vec<String>) -> Result<String, String> {
    match words.len() {
        0 => Err("missing <url>".into()),
        1 => Ok(words.remove(0)),
        _ => Err(format!("unexpected argument {}", words[1])),
    }
}

#[derive(Debug, PartialEq)]
struct ScrapeOpts {
    url: String,
    focus: Option<String>,
    max_chars: usize,
    offset: Option<usize>,
    json: bool,
    stats: bool,
}

fn parse_scrape(args: &[String]) -> Result<ScrapeOpts, String> {
    let mut o = ScrapeOpts {
        url: String::new(),
        focus: None,
        max_chars: 6000,
        offset: None,
        json: false,
        stats: false,
    };
    let mut words = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--max-chars" => o.max_chars = parse_pos(flag_value(args, &mut i)?, "--max-chars")?,
            "--offset" => o.offset = Some(parse_num(flag_value(args, &mut i)?, "--offset")?),
            "--focus" => o.focus = Some(flag_value(args, &mut i)?.to_string()),
            "--json" => o.json = true,
            "--stats" => o.stats = true,
            s if s.starts_with('-') => return Err(format!("unknown flag {s}")),
            s => words.push(s.to_string()),
        }
        i += 1;
    }
    if words.is_empty() {
        return Err("missing <url>".into());
    }
    o.url = words.remove(0);
    // Positional focus words and --focus merge into one query.
    if !words.is_empty() {
        let rest = words.join(" ");
        o.focus = Some(match o.focus.take() {
            Some(f) => format!("{f} {rest}"),
            None => rest,
        });
    }
    Ok(o)
}

#[derive(Debug, PartialEq)]
struct BatchOpts {
    urls: Vec<String>,
    focus: Option<String>,
    max_chars: usize,
    parallel: usize,
    json: bool,
    stats: bool,
}

fn parse_batch(args: &[String]) -> Result<BatchOpts, String> {
    let mut o = BatchOpts {
        urls: Vec::new(),
        focus: None,
        max_chars: 6000,
        parallel: 4,
        json: false,
        stats: false,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--focus" => o.focus = Some(flag_value(args, &mut i)?.to_string()),
            "--max-chars" => o.max_chars = parse_pos(flag_value(args, &mut i)?, "--max-chars")?,
            "--parallel" => o.parallel = parse_num(flag_value(args, &mut i)?, "--parallel")?,
            "--json" => o.json = true,
            "--stats" => o.stats = true,
            s if s.starts_with('-') => return Err(format!("unknown flag {s}")),
            s => o.urls.push(s.to_string()),
        }
        i += 1;
    }
    Ok(o)
}

/// Batch URLs from stdin: one per line; blanks and `#` comments are skipped.
fn parse_url_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

#[derive(Debug, PartialEq)]
struct SearchOpts {
    query: String,
    max: usize,
    site: Option<String>,
    freshness: Option<String>,
    fetch: usize,
    focus: String,
    max_chars: usize,
    json: bool,
    stats: bool,
}

fn parse_search(args: &[String]) -> Result<SearchOpts, String> {
    let mut o = SearchOpts {
        query: String::new(),
        max: 10,
        site: None,
        freshness: None,
        fetch: 0,
        focus: String::new(),
        max_chars: 2500,
        json: false,
        stats: false,
    };
    let mut words = Vec::new();
    let mut focus = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--max" => o.max = parse_num(flag_value(args, &mut i)?, "--max")?,
            "--site" => o.site = Some(flag_value(args, &mut i)?.to_string()),
            "--freshness" => o.freshness = Some(flag_value(args, &mut i)?.to_string()),
            "--fetch" => o.fetch = parse_num::<usize>(flag_value(args, &mut i)?, "--fetch")?.min(8),
            "--focus" => focus = Some(flag_value(args, &mut i)?.to_string()),
            "--max-chars" => o.max_chars = parse_pos(flag_value(args, &mut i)?, "--max-chars")?,
            "--json" => o.json = true,
            "--stats" => o.stats = true,
            s if s.starts_with('-') => return Err(format!("unknown flag {s}")),
            s => words.push(s.to_string()),
        }
        i += 1;
    }
    if words.is_empty() {
        return Err("missing <query>".into());
    }
    o.query = words.join(" ");
    o.focus = focus.unwrap_or_else(|| o.query.clone());
    o.max = o.max.clamp(1, 20);
    Ok(o)
}

#[derive(Debug, PartialEq)]
struct LinksOpts {
    url: String,
    filter: Option<String>,
    same_site: bool,
    max: usize,
}

fn parse_links(args: &[String]) -> Result<LinksOpts, String> {
    let mut words = Vec::new();
    let mut filter = None;
    let mut same_site = false;
    let mut max = 100;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--filter" => filter = Some(flag_value(args, &mut i)?.to_string()),
            "--same-site" => same_site = true,
            "--max" => max = parse_pos(flag_value(args, &mut i)?, "--max")?,
            s if s.starts_with('-') => return Err(format!("unknown flag {s}")),
            s => words.push(s.to_string()),
        }
        i += 1;
    }
    Ok(LinksOpts {
        url: one_url(words)?,
        filter,
        same_site,
        max,
    })
}

#[derive(Debug, PartialEq)]
struct CrawlOpts {
    url: String,
    max_pages: usize,
    depth: usize,
    focus: Option<String>,
    max_chars: usize,
    sitemap: bool,
    json: bool,
    stats: bool,
}

fn parse_crawl(args: &[String]) -> Result<CrawlOpts, String> {
    let mut words = Vec::new();
    let mut o = CrawlOpts {
        url: String::new(),
        max_pages: 10,
        depth: 2,
        focus: None,
        max_chars: 2500,
        sitemap: false,
        json: false,
        stats: false,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--max-pages" => o.max_pages = parse_num(flag_value(args, &mut i)?, "--max-pages")?,
            "--depth" => o.depth = parse_num(flag_value(args, &mut i)?, "--depth")?,
            "--focus" => o.focus = Some(flag_value(args, &mut i)?.to_string()),
            "--max-chars" => o.max_chars = parse_pos(flag_value(args, &mut i)?, "--max-chars")?,
            "--sitemap" => o.sitemap = true,
            "--json" => o.json = true,
            "--stats" => o.stats = true,
            s if s.starts_with('-') => return Err(format!("unknown flag {s}")),
            s => words.push(s.to_string()),
        }
        i += 1;
    }
    o.url = one_url(words)?;
    o.max_pages = o.max_pages.clamp(1, 50);
    Ok(o)
}

/// `(url, -o path)`.
fn parse_screenshot(args: &[String]) -> Result<(String, Option<String>), String> {
    let mut words = Vec::new();
    let mut out = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" => out = Some(flag_value(args, &mut i)?.to_string()),
            s if s.starts_with('-') => return Err(format!("unknown flag {s}")),
            s => words.push(s.to_string()),
        }
        i += 1;
    }
    Ok((one_url(words)?, out))
}

/// `links`: dedupe by href, optional case-insensitive substring on href or
/// text, optional same-host filter, capped at `max`. Returns (text, href).
fn filter_links(
    links: &[extractor::Link],
    page_url: &str,
    filter: Option<&str>,
    same_site: bool,
    max: usize,
) -> Vec<(String, String)> {
    let page_host = url::Url::parse(page_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_lowercase));
    let needle = filter.map(str::to_lowercase);
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for l in links {
        if out.len() >= max {
            break;
        }
        if seen.contains(&l.href) {
            continue;
        }
        let text = l.text.split_whitespace().collect::<Vec<_>>().join(" ");
        if let Some(n) = &needle
            && !l.href.to_lowercase().contains(n.as_str())
            && !text.to_lowercase().contains(n.as_str())
        {
            continue;
        }
        if same_site {
            let host = url::Url::parse(&l.href)
                .ok()
                .and_then(|u| u.host_str().map(str::to_lowercase));
            if host.is_none() || host != page_host {
                continue;
            }
        }
        seen.insert(l.href.clone());
        let text: String = if text.is_empty() {
            "(no text)".to_string()
        } else {
            text.chars().take(100).collect()
        };
        out.push((text, l.href.clone()));
    }
    out
}

/// `gh`/`pkg` lookups printed in input order, blank-line separated. Failures go to
/// stderr as `error: <key>: <msg>`. Returns how many failed.
fn print_lookups(keys: &[String], results: Vec<anyhow::Result<String>>) -> usize {
    let mut failed = 0;
    let mut first = true;
    for (key, res) in keys.iter().zip(results) {
        match res {
            Ok(md) => {
                if !first {
                    println!();
                }
                first = false;
                println!("{}", md.trim_end());
            }
            Err(e) => {
                failed += 1;
                eprintln!("error: {key}: {e}");
            }
        }
    }
    failed
}

/// One fetched URL: input URL plus its scrape (or error), in input order.
pub(crate) type PageFetch = (String, anyhow::Result<(ScrapeResult, Stats)>);

/// Scrapes `urls` concurrently (at most `parallel`, clamped 1..=8). One entry per input URL,
/// in input order: a failed or panicked task is an `Err` entry, never dropped.
const PAGE_DEADLINE: Duration = Duration::from_secs(25);

pub(crate) async fn fetch_all(
    urls: Vec<String>,
    focus: Option<String>,
    max_chars: usize,
    parallel: usize,
    op: &'static str,
) -> Vec<PageFetch> {
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(parallel.clamp(1, 8)));
    let handles: Vec<_> = urls
        .iter()
        .cloned()
        .map(|url| {
            let sem = sem.clone();
            let focus = focus.clone();
            tokio::spawn(async move {
                let _permit = sem.acquire_owned().await;
                // One dead host must not hold up the whole batch.
                let fut = run_ext_stats(
                    &url,
                    focus.as_deref(),
                    max_chars,
                    true,
                    None,
                    false,
                    None,
                    None,
                );
                tokio::time::timeout(PAGE_DEADLINE, fut)
                    .await
                    .unwrap_or_else(|_| {
                        Err(anyhow::anyhow!(
                            "timed out after {}s",
                            PAGE_DEADLINE.as_secs()
                        ))
                    })
                    .map(|(r, mut st)| {
                        st.op = op;
                        (r, st)
                    })
            })
        })
        .collect();
    let mut out = Vec::with_capacity(handles.len());
    for (url, handle) in urls.into_iter().zip(handles) {
        let res = handle
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!("fetch task failed: {e}")));
        out.push((url, res));
    }
    out
}

/// Markdown section for one scraped page (`## url`, meta comment, body).
fn fetched_md(r: &ScrapeResult, focus: Option<&str>, max_chars: usize) -> String {
    format!(
        "## {}\n<!-- tier: {} content_ok: {} chars: {} -->\n\n{}",
        r.url,
        r.tier,
        r.content_ok,
        r.total_chars,
        with_hint(
            &r.markdown,
            &r.url,
            r.next_offset,
            r.focused_chars,
            focus,
            max_chars
        )
    )
}

/// Markdown section for a failed fetch (`## url`, error comment).
fn print_failed(url: &str, e: &anyhow::Error) {
    println!("## {url}\n<!-- error: {e} -->\n\n_fetch failed_\n");
}

fn print_pages_md(pages: &[PageFetch], focus: Option<&str>, max_chars: usize) {
    for (url, res) in pages {
        match res {
            Ok((r, _)) => println!("{}\n", fetched_md(r, focus, max_chars)),
            Err(e) => print_failed(url, e),
        }
    }
}

/// `{url, content_ok:false, error}` for a failed fetch.
fn error_json(url: &str, e: &anyhow::Error) -> serde_json::Value {
    serde_json::json!({"url": url, "content_ok": false, "error": e.to_string()})
}

/// JSON for one page; failed fetches become `error_json`.
fn page_json(url: &str, res: &anyhow::Result<(ScrapeResult, Stats)>) -> serde_json::Value {
    match res {
        Ok((r, _)) => serde_json::to_value(r).unwrap_or_default(),
        Err(e) => error_json(url, e),
    }
}

/// True when there were pages and every fetch failed (exit 1).
fn all_failed(pages: &[PageFetch]) -> bool {
    !pages.is_empty() && pages.iter().all(|(_, r)| r.is_err())
}

fn print_item_stats(pages: &[PageFetch]) {
    for (_, res) in pages {
        if let Ok((_, st)) = res {
            st.print();
        }
    }
}

pub(crate) async fn cmd_scrape(args: &[String]) -> anyhow::Result<()> {
    let o = parse_scrape(args).unwrap_or_else(|e| usage(&e));
    match run_ext_stats(
        &o.url,
        o.focus.as_deref(),
        o.max_chars,
        true,
        o.offset,
        false,
        None,
        None,
    )
    .await
    {
        Ok((r, st)) => {
            if o.json {
                println!("{}", serde_json::to_string(&r)?);
            } else {
                emit_scrape(&r, o.focus.as_deref(), o.max_chars);
            }
            if o.stats {
                st.print();
            }
        }
        // With --json the failure is data on stdout; exit 1 either way.
        Err(e) if o.json => {
            println!("{}", error_json(&o.url, &e));
            std::process::exit(1);
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

pub(crate) async fn cmd_search(args: &[String]) -> anyhow::Result<()> {
    let o = parse_search(args).unwrap_or_else(|e| usage(&e));
    let t0 = std::time::Instant::now();
    let (hits, engines, warning) = search_run(
        &o.query,
        o.max,
        o.site.as_deref(),
        None,
        o.freshness.as_deref(),
    )
    .await?;
    let ms = t0.elapsed().as_millis();
    let pages = if o.fetch > 0 {
        let urls: Vec<String> = hits.iter().take(o.fetch).map(|h| h.url.clone()).collect();
        fetch_all(urls, Some(o.focus.clone()), o.max_chars, 4, "search_fetch").await
    } else {
        Vec::new()
    };
    let sr = SearchResult {
        query: o.query.clone(),
        total: hits.len(),
        hits,
        engines,
        warning,
    };
    if o.json {
        let mut v = serde_json::to_value(&sr)?;
        if o.fetch > 0 {
            v["pages"] = pages.iter().map(|(u, r)| page_json(u, r)).collect();
        }
        println!("{v}");
    } else {
        println!("# {}", sr.query);
        if let Some(w) = &sr.warning {
            println!("<!-- warning: {w} -->");
        }
        for (idx, h) in sr.hits.iter().enumerate() {
            println!("{}. {} — {}", idx + 1, h.title, h.url);
            if !h.snippet.is_empty() {
                println!("   {}", h.snippet);
            }
        }
        if o.fetch > 0 {
            println!();
            print_pages_md(&pages, Some(o.focus.as_str()), o.max_chars);
        }
    }
    if o.stats {
        print_item_stats(&pages);
        eprintln!(
            "stats: {}",
            serde_json::json!({"op":"search","query":&o.query,"hits":sr.total,"ms":ms,"engines":&sr.engines})
        );
    }
    if all_failed(&pages) {
        std::process::exit(1);
    }
    Ok(())
}

pub(crate) async fn cmd_batch(args: &[String]) -> anyhow::Result<()> {
    let mut o = parse_batch(args).unwrap_or_else(|e| usage(&e));
    if o.urls.is_empty() {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        o.urls = parse_url_lines(&buf);
    }
    o.urls = cap_inputs(o.urls, "batch");
    if o.urls.is_empty() {
        usage("no URLs given (args or stdin)");
    }
    let n = o.urls.len();
    let t0 = std::time::Instant::now();
    let pages = fetch_all(
        o.urls,
        o.focus.clone(),
        o.max_chars,
        o.parallel,
        "batch_item",
    )
    .await;
    if o.json {
        let arr: Vec<serde_json::Value> = pages.iter().map(|(u, r)| page_json(u, r)).collect();
        println!("{}", serde_json::Value::Array(arr));
    } else {
        print_pages_md(&pages, o.focus.as_deref(), o.max_chars);
    }
    if o.stats {
        print_item_stats(&pages);
        let ok = pages
            .iter()
            .filter(|(_, r)| matches!(r, Ok((d, _)) if d.content_ok))
            .count();
        let chars: usize = pages
            .iter()
            .filter_map(|(_, r)| r.as_ref().ok())
            .map(|(d, _)| d.markdown.chars().count())
            .sum();
        eprintln!(
            "stats: {}",
            serde_json::json!({"op":"batch","n":n,"ms":t0.elapsed().as_millis(),"chars_out":chars,"est_tokens":chars/4,"ok":ok})
        );
    }
    if all_failed(&pages) {
        std::process::exit(1);
    }
    Ok(())
}

pub(crate) async fn cmd_gh(args: &[String]) -> anyhow::Result<()> {
    let mut o = parse_gh(args).unwrap_or_else(|e| usage(&e));
    let specs = cap_inputs(std::mem::take(&mut o.specs), "gh");
    if specs.is_empty() {
        usage("gh needs at least one owner/repo or github URL");
    }
    let o = &o;
    // At most 4 specs in flight: GitHub's unauthenticated quota is 60 requests/hour.
    use futures::StreamExt;
    let res: Vec<anyhow::Result<String>> =
        futures::stream::iter(specs.iter().map(|s| gh_one(s, o)))
            .buffered(4)
            .collect()
            .await;
    if print_lookups(&specs, res) > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// One spec's output: repo summary, plus the README excerpt when asked. A README failure is
/// reported inline; only the repo lookup decides success.
async fn gh_one(spec: &str, o: &GhOpts) -> anyhow::Result<String> {
    gh_summary(spec, o.readme, o.focus.as_deref(), o.max_chars).await
}

/// Shared GitHub repo summary used by the CLI `gh` subcommand and the MCP `github_repo` tool.
/// Returns the repo markdown plus, when `readme`, a focus-filtered, size-capped README excerpt.
pub(crate) async fn gh_summary(
    spec: &str,
    readme: bool,
    focus: Option<&str>,
    max_chars: usize,
) -> anyhow::Result<String> {
    let (repo, readme) = tools::gh_with_readme(spec, readme).await?;
    let Some(readme) = readme else {
        return Ok(repo);
    };
    let excerpt = match readme {
        Ok(text) => {
            let (text, missed) = match focus {
                Some(q) => match focus_filter(&text, q, 5) {
                    Some(t) => (t, false),
                    None => (text, true),
                },
                None => (text, false),
            };
            let (body, next) = cut_page(&text, missed, 0, max_chars);
            let marker = if next.is_some() {
                "\n\n<!-- readme truncated -->"
            } else {
                ""
            };
            format!("## README (excerpt)\n{}{marker}", body.trim_end())
        }
        Err(e) => format!("<!-- readme unavailable: {e} -->"),
    };
    Ok(format!("{repo}\n\n{excerpt}"))
}

#[derive(Debug, PartialEq)]
struct GhOpts {
    specs: Vec<String>,
    readme: bool,
    focus: Option<String>,
    max_chars: usize,
}

fn parse_gh(args: &[String]) -> Result<GhOpts, String> {
    let mut specs = Vec::new();
    let mut readme = false;
    let mut focus = None;
    let mut max_chars = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--readme" => readme = true,
            "--focus" => focus = Some(flag_value(args, &mut i)?.to_string()),
            "--max-chars" => max_chars = Some(parse_pos(flag_value(args, &mut i)?, "--max-chars")?),
            s if s.starts_with('-') => return Err(format!("unknown flag {s}")),
            s => specs.push(s.to_string()),
        }
        i += 1;
    }
    if !readme && (focus.is_some() || max_chars.is_some()) {
        return Err("--focus and --max-chars need --readme".into());
    }
    Ok(GhOpts {
        specs,
        readme,
        focus,
        max_chars: max_chars.unwrap_or(1500),
    })
}

/// Registry names `tools::pkg` accepts (case-insensitive, surrounding spaces ignored).
fn is_pkg_registry(r: &str) -> bool {
    matches!(
        r.trim().to_ascii_lowercase().as_str(),
        "crates" | "crate" | "crates.io" | "npm" | "npmjs" | "pypi" | "pip"
    )
}

pub(crate) async fn cmd_pkg(args: &[String]) -> anyhow::Result<()> {
    let mut words = positionals(args).unwrap_or_else(|e| usage(&e));
    if words.len() < 2 {
        usage("pkg needs a registry (crates|npm|pypi) and at least one name");
    }
    let registry = words.remove(0);
    if !is_pkg_registry(&registry) {
        usage(&format!(
            "unknown registry '{registry}' (valid: crates, npm, pypi)"
        ));
    }
    let names = cap_inputs(words, "pkg");
    let res = futures::future::join_all(names.iter().map(|n| tools::pkg(&registry, n))).await;
    if print_lookups(&names, res) > 0 {
        std::process::exit(1);
    }
    Ok(())
}

pub(crate) async fn cmd_links(args: &[String]) -> anyhow::Result<()> {
    let o = parse_links(args).unwrap_or_else(|e| usage(&e));
    validate(&o.url)?;
    let got = fetcher::fetch(&o.url).await?;
    if got.blocked {
        eprintln!(
            "warning: content_ok false ({}); links may come from a block page",
            got.reason.as_deref().unwrap_or("blocked")
        );
    }
    for (text, href) in filter_links(
        &page_links(&got.html, &o.url),
        &o.url,
        o.filter.as_deref(),
        o.same_site,
        o.max,
    ) {
        println!("{text} — {href}");
    }
    Ok(())
}

pub(crate) async fn cmd_crawl(args: &[String]) -> anyhow::Result<()> {
    let o = parse_crawl(args).unwrap_or_else(|e| usage(&e));
    let t0 = std::time::Instant::now();
    let res = crawl_run(
        &o.url,
        o.max_pages,
        o.depth,
        o.focus.as_deref(),
        o.max_chars,
        Some(o.sitemap),
        None,
        None,
    )
    .await?;
    if o.json {
        let mut v = serde_json::to_value(&res)?;
        if let Some(items) = v["pages"].as_array_mut() {
            items.extend(res.failed.iter().map(|(u, e)| error_json(u, e)));
        }
        println!("{v}");
    } else {
        for p in &res.pages {
            println!("{}\n", fetched_md(p, o.focus.as_deref(), o.max_chars));
        }
        for (url, e) in &res.failed {
            print_failed(url, e);
        }
    }
    if o.stats {
        eprintln!(
            "stats: {}",
            serde_json::json!({"op":"crawl","start_url":&o.url,"pages":res.pages.len(),"visited":res.visited.len(),"total_chars":res.total_chars,"ms":t0.elapsed().as_millis()})
        );
    }
    Ok(())
}

pub(crate) async fn cmd_screenshot(args: &[String]) -> anyhow::Result<()> {
    let (url, out) = parse_screenshot(args).unwrap_or_else(|e| usage(&e));
    let png = screenshot_png(&url).await?;
    let path = out.unwrap_or_else(|| {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("/tmp/scour-{ts}.png")
    });
    std::fs::write(&path, &png)?;
    println!("{path}");
    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;
    use crate::scrape::{slice_at_offset, truncate};

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn help_flags_detected() {
        for h in ["--help", "-h", "help"] {
            assert!(is_help(Some(h)));
        }
        assert!(!is_help(None));
        assert!(!is_help(Some("https://example.com")));
        assert!(!is_help(Some("scrape")));
    }

    #[test]
    fn search_fetch_defaults() {
        let o = parse_search(&s(&["axum middleware", "--fetch", "2"])).unwrap();
        assert_eq!((o.fetch, o.max, o.max_chars), (2, 10, 2500));
        assert_eq!(o.focus, "axum middleware");
        assert_eq!(parse_search(&s(&["x", "--fetch", "50"])).unwrap().fetch, 8);
        let o = parse_search(&s(&["x", "--focus", "routing", "--max-chars", "300"])).unwrap();
        assert_eq!((o.focus.as_str(), o.max_chars), ("routing", 300));
        assert!(parse_search(&s(&["x", "--bogus"])).is_err());
        assert!(parse_search(&s(&["--fetch", "2"])).is_err());
    }

    #[test]
    fn gh_flags_parse() {
        let o = parse_gh(&s(&["tokio-rs/axum", "actix/actix-web"])).unwrap();
        assert_eq!((o.specs.len(), o.readme, o.max_chars), (2, false, 1500));
        assert_eq!(o.focus, None);
        let o = parse_gh(&s(&[
            "tokio-rs/axum",
            "--readme",
            "--focus",
            "middleware tower",
            "--max-chars",
            "600",
        ]))
        .unwrap();
        assert_eq!(
            o,
            GhOpts {
                specs: s(&["tokio-rs/axum"]),
                readme: true,
                focus: Some("middleware tower".into()),
                max_chars: 600
            }
        );
        assert!(parse_gh(&s(&["x/y", "--focus", "q"])).is_err());
        assert!(parse_gh(&s(&["x/y", "--max-chars", "300"])).is_err());
        assert!(parse_gh(&s(&["x/y", "--readme", "--max-chars", "0"])).is_err());
        assert!(parse_gh(&s(&["x/y", "--readme", "--bogus"])).is_err());
        assert!(parse_gh(&s(&["x/y", "--readme", "--focus"])).is_err());
    }

    #[test]
    fn links_filter_dedupes_and_scopes() {
        let l = |href: &str, text: &str| extractor::Link {
            href: href.into(),
            text: text.into(),
        };
        let links = vec![
            l(
                "https://docs.rs/axum/latest/axum/middleware/index.html",
                "Middleware",
            ),
            l(
                "https://docs.rs/axum/latest/axum/middleware/index.html",
                "Middleware",
            ),
            l("https://github.com/tokio-rs/axum", "GitHub"),
            l(
                "https://docs.rs/axum/latest/axum/routing/",
                "  Routing\n  Guide ",
            ),
        ];
        let page = "https://docs.rs/axum/latest/axum/";
        let all = filter_links(&links, page, None, false, 100);
        assert_eq!(all.len(), 3);
        assert_eq!(all[2].0, "Routing Guide");
        assert_eq!(
            filter_links(&links, page, Some("MIDDLEWARE"), false, 100).len(),
            1
        );
        assert_eq!(
            filter_links(&links, page, Some("github"), false, 100).len(),
            1
        );
        let same = filter_links(&links, page, None, true, 100);
        assert_eq!(same.len(), 2);
        assert!(same.iter().all(|(_, h)| h.starts_with("https://docs.rs")));
        assert_eq!(filter_links(&links, page, None, false, 1).len(), 1);
    }

    #[test]
    fn page_links_resolves_strips_fragments_and_dedupes() {
        let html = r##"<a href="/x">X</a><a href="/x#a">again</a><a href="#top">top</a><a href="javascript:void(0)">j</a><a href="mailto:a@b.c">m</a><a href="https://o.com/y#z"></a>"##;
        let hrefs: Vec<String> = page_links(html, "https://e.com/a/")
            .into_iter()
            .map(|l| l.href)
            .collect();
        assert_eq!(hrefs, vec!["https://e.com/x", "https://o.com/y"]);
    }

    #[test]
    fn truncation_hint_is_pasteable() {
        assert_eq!(
            with_hint("abc", "https://e.com/a", None, 0, None, 300),
            "abc"
        );
        assert_eq!(
            with_hint(
                "abc",
                "https://e.com/a",
                Some(150),
                900,
                Some("rust traits"),
                300
            ),
            "abc\n\n<!-- truncated at 150 of 900 chars; more: scour scrape 'https://e.com/a' --offset 150 --max-chars 300 --focus 'rust traits' -->"
        );
        assert_eq!(
            with_hint("x", "https://e.com/it's", Some(5), 9, None, 6000),
            "x\n\n<!-- truncated at 5 of 9 chars; more: scour scrape 'https://e.com/it'\\''s' --offset 5 --max-chars 6000 -->"
        );
        assert_eq!(sh_single("a \"b\" $c `d` !e"), "'a \"b\" $c `d` !e'");
        assert!(
            with_hint("x", "https://e.com", Some(1), 2, Some("hi!"), 9).contains("--focus 'hi!'")
        );
    }

    #[test]
    fn focus_merges_flag_and_words() {
        let o = parse_scrape(&s(&["https://e.com", "traits", "--focus", "rust"])).unwrap();
        assert_eq!(o.focus.as_deref(), Some("rust traits"));
        let o = parse_scrape(&s(&["https://e.com", "--focus", "rust", "traits"])).unwrap();
        assert_eq!(o.focus.as_deref(), Some("rust traits"));
    }

    #[test]
    fn links_and_pkg_usage_errors() {
        assert!(parse_links(&s(&["https://e.com", "--max", "0"])).is_err());
        assert_eq!(
            parse_links(&s(&["https://e.com", "--max", "3"]))
                .unwrap()
                .max,
            3
        );
        assert!(is_pkg_registry("NPM") && is_pkg_registry(" pip ") && is_pkg_registry("crates.io"));
        assert!(!is_pkg_registry("maven"));
    }

    #[test]
    fn hint_offset_resumes_where_truncation_stopped() {
        let md = "para one\n\npara two is here\n\npara three";
        let (first, cut) = truncate(md, 12);
        assert!(cut);
        assert_eq!(first, "para one");
        assert!(slice_at_offset(md, first.chars().count()).starts_with("\n\npara two"));
    }

    #[test]
    fn truncate_counts_chars_and_always_advances() {
        let (out, cut) = truncate(&"é".repeat(10), 5);
        assert!(cut);
        assert_eq!(out, "é".repeat(5));
        assert_eq!(truncate("hello", 0), ("h".to_string(), true));
        assert_eq!(truncate("", 0), (String::new(), false));
        assert_eq!(slice_at_offset("日本語テキスト", 3), "テキスト");
    }

    #[test]
    fn max_chars_zero_is_a_usage_error() {
        assert!(parse_scrape(&s(&["https://e.com", "--max-chars", "0"])).is_err());
        assert!(parse_batch(&s(&["https://e.com", "--max-chars", "0"])).is_err());
        assert!(parse_search(&s(&["x", "--max-chars", "0"])).is_err());
        assert!(parse_crawl(&s(&["https://e.com", "--max-chars", "0"])).is_err());
        assert_eq!(
            parse_scrape(&s(&["https://e.com", "--max-chars", "1"]))
                .unwrap()
                .max_chars,
            1
        );
    }

    #[test]
    fn bare_word_is_not_a_url() {
        assert!(is_bare_word("serch"));
        assert!(!is_bare_word("example.com"));
        assert!(!is_bare_word("https://example.com"));
        assert!(!is_bare_word("--json"));
    }

    #[test]
    fn scrape_focus_flag() {
        let o = parse_scrape(&s(&["https://e.com", "--focus", "rust traits"])).unwrap();
        assert_eq!(o.focus.as_deref(), Some("rust traits"));
    }

    #[test]
    fn scrape_parses_offset_focus_and_rejects_unknown() {
        let o = parse_scrape(&s(&[
            "https://e.com",
            "rust",
            "traits",
            "--offset",
            "6000",
            "--json",
        ]))
        .unwrap();
        assert_eq!(o.url, "https://e.com");
        assert_eq!(o.focus.as_deref(), Some("rust traits"));
        assert_eq!((o.offset, o.max_chars, o.json), (Some(6000), 6000, true));
        assert!(parse_scrape(&s(&["https://e.com", "--nope"])).is_err());
        assert!(parse_scrape(&s(&["--stats"])).is_err());
    }

    #[test]
    fn stdin_urls_trimmed_and_comments_skipped() {
        let got = parse_url_lines("  https://a.com \n\n# note\nhttps://b.com\n   # indented\n");
        assert_eq!(got, vec!["https://a.com", "https://b.com"]);
    }

    #[test]
    fn crawl_caps_max_pages() {
        assert_eq!(
            parse_crawl(&s(&["https://e.com", "--max-pages", "999"]))
                .unwrap()
                .max_pages,
            50
        );
        assert_eq!(parse_crawl(&s(&["https://e.com"])).unwrap().max_pages, 10);
    }
}
