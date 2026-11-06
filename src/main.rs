//! scour — local MCP scraper + CLI. HTTP first, Chrome only when blocked.
//!
//!   scour            -> MCP server on stdio (for Pi / Claude)
//!   scour <url>      -> print markdown (smoke test, no MCP client needed)
//!   scour --help     -> list every CLI subcommand
//!
//! Frontier upgrade: article + list fallback, links/page_type, BM25 focus,
//! batch parallelism, and in-domain crawl.

mod cache;
mod cli;
mod extractor;
mod fetcher;
mod mcp;
mod scrape;
mod search;
mod tools;
mod types;

use cli::{
    USAGE, cmd_batch, cmd_crawl, cmd_gh, cmd_links, cmd_pkg, cmd_scrape, cmd_screenshot,
    cmd_search, is_bare_word, is_help, usage,
};
use mcp::{Scour, version_info};

/// Exit right after a CLI command. Returning from main would make the tokio
/// runtime wait for timed-out headless-Chrome `spawn_blocking` work.
fn finish(r: anyhow::Result<()>) -> ! {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    match r {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("Error: {e:?}");
            std::process::exit(1)
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str);
    if is_help(cmd) {
        print!("{USAGE}");
        return Ok(());
    }
    match cmd {
        None => {}
        Some("--version" | "-V") => {
            println!(
                "{}",
                serde_json::to_string(&version_info()).unwrap_or_default()
            );
            return Ok(());
        }
        Some("scrape") => finish(cmd_scrape(&args[1..]).await),
        Some("search") => finish(cmd_search(&args[1..]).await),
        Some("batch") => finish(cmd_batch(&args[1..]).await),
        Some("gh") => finish(cmd_gh(&args[1..]).await),
        Some("pkg") => finish(cmd_pkg(&args[1..]).await),
        Some("links") => finish(cmd_links(&args[1..]).await),
        Some("crawl") => finish(cmd_crawl(&args[1..]).await),
        Some("screenshot") => finish(cmd_screenshot(&args[1..]).await),
        Some("cache-clear") => {
            cache::cache_clear();
            eprintln!("cache cleared");
            return Ok(());
        }
        // A mistyped subcommand (`serch x`) is not a URL.
        Some(s) if is_bare_word(s) => usage(&format!("unknown command '{s}'")),
        // Bare URL or leading flags: shorthand for `scrape <url> [focus...] [flags]`.
        Some(_) => finish(cmd_scrape(&args).await),
    }
    use rmcp::ServiceExt;
    let service = Scour::new().serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
