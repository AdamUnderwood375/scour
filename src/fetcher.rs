//! Cheapest-first fetch ladder: plain HTTP -> headless Chrome.
//! Tier 1 handles ~80% of the web at ~zero cost; Chrome only pays its tax when
//! the cheap tier is visibly blocked (Cloudflare / JS-rendered shell).

use anyhow::{Result, anyhow};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Which rung of the ladder produced the HTML. Returned to the agent so it can
/// reason about cost/trust without guessing.
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Http,
    Chrome,
}

pub struct Fetched {
    pub html: String,
    pub tier: Tier,
    pub status: u16,
    /// True when every tier still returned a challenge/shell page. The agent
    /// must be told this rather than handed an interstitial as if it were
    /// content.
    pub blocked: bool,
    /// Why the final tier looked blocked, in plain words.
    pub reason: Option<String>,
    /// Why the *cheap* tier looked blocked — kept after escalation so we can
    /// say what we were escalating for.
    pub first_reason: Option<String>,
    /// True when the cheap tier was refused and we had to escalate. If a page
    /// we escalated *for* still yields no article, we did not actually get
    /// through — see `run()`. Without this, a browser that renders a bot
    /// shell looks identical to success.
    pub escalated: bool,
    /// Status from the cheap tier, kept for diagnostics after escalation.
    #[allow(dead_code)]
    pub first_status: u16,
}

pub fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            // Real-ish Chrome UA. Not TLS-fingerprint spoofing (that needs
            // reqwest-impersonate); enough for plain bot checks.
            .user_agent(
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
            )
            .cookie_store(true)
            .timeout(Duration::from_secs(20))
            .build()
            .expect("http client")
    })
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Bytes of visible (non-whitespace, non-markup) text, and bytes locked inside
/// `<script>`/`<style>` blocks. One pass, no parser.
///
/// This is the language-independent half of block detection: a bot wall is
/// structurally a few words wrapped in a lot of JavaScript, whatever language
/// it apologises in.
fn visible_and_script_len(html: &str) -> (usize, usize) {
    let lower = html.to_ascii_lowercase();
    let b = lower.as_bytes();
    let (mut visible, mut script, mut i) = (0usize, 0usize, 0usize);
    'scan: while i < b.len() {
        if b[i] == b'<' {
            let rest = &b[i..];
            for (open, close) in [
                (&b"<script"[..], &b"</script"[..]),
                (&b"<style"[..], &b"</style"[..]),
            ] {
                if rest.starts_with(open) {
                    let end = find_sub(rest, close).unwrap_or(rest.len());
                    script += end;
                    i += end.max(1);
                    continue 'scan;
                }
            }
            i += rest
                .iter()
                .position(|&c| c == b'>')
                .map(|p| p + 1)
                .unwrap_or(rest.len());
        } else {
            if !b[i].is_ascii_whitespace() {
                visible += 1;
            }
            i += 1;
        }
    }
    (visible, script)
}

/// Why this response looks like a wall rather than content, if it does.
///
/// Three layers, cheapest first. The last one is the important one: it needs
/// no vendor list and no English, so a protection service we have never seen
/// still gets caught.
///
/// ponytail: heuristic, not a classifier. A page that is genuinely a few words
/// plus heavy JS reads as a shell — which costs one browser escalation, not a
/// wrong answer. Upgrade path is checking for rendered text after escalation,
/// which `run()` already does.
pub fn block_reason(status: u16, html: &str) -> Option<String> {
    if matches!(status, 401 | 403 | 407 | 429 | 503) {
        return Some(format!("HTTP {status} from origin"));
    }
    if html.len() < 500 {
        return Some("near-empty response body".into());
    }

    // Interstitials are small. A 200KB document is real content even if it
    // happens to contain one of these phrases in its prose.
    if html.len() < 60_000 {
        let head: String = html.chars().take(16_384).collect::<String>().to_lowercase();

        // Infrastructure that only ships on a challenge page. Independent of
        // the page's language, unlike the copy below.
        const INFRA: [(&str, &str); 6] = [
            ("/cdn-cgi/challenge-platform", "Cloudflare"),
            ("captcha-delivery.com", "DataDome"),
            ("_incapsula_resource", "Imperva"),
            ("px-captcha", "PerimeterX"),
            ("/_sec/cp_challenge", "Akamai"),
            ("geo.captcha", "DataDome"),
        ];
        if let Some((_, vendor)) = INFRA.iter().find(|(n, _)| head.contains(n)) {
            return Some(format!("{vendor} challenge page"));
        }

        const COPY: [&str; 10] = [
            "just a moment",
            "cf-browser-verification",
            "checking your browser",
            "cf_chl_opt",
            "enable javascript and cookies to continue",
            "attention required! | cloudflare",
            "__cf_chl",
            "please enable js",
            "please enable javascript",
            "verifying you are human",
        ];
        if COPY.iter().any(|m| head.contains(m)) {
            return Some("bot interstitial text".into());
        }
    }

    // Vendor-agnostic catch-all: almost no text, mostly script.
    let (visible, script) = visible_and_script_len(html);
    if visible < 500 && script > 500 && script > visible.saturating_mul(2) {
        return Some(format!(
            "thin body dominated by scripts ({visible}B text vs {script}B script) \
             - challenge page or client-rendered shell"
        ));
    }
    None
}

async fn fetch_http(url: &str) -> Result<Fetched> {
    let res = client().get(url).send().await?;
    let status = res.status().as_u16();
    let html = res.text().await?;
    let reason = block_reason(status, &html);
    Ok(Fetched {
        blocked: reason.is_some(),
        first_reason: reason.clone(),
        reason,
        html,
        tier: Tier::Http,
        status,
        escalated: false,
        first_status: status,
    })
}

/// Reuse the browser already on this machine instead of downloading another.
/// Brave is Chromium-based, so it speaks the same DevTools protocol.
fn browser_path() -> Result<std::path::PathBuf> {
    if let Ok(p) = std::env::var("SCOUR_BROWSER") {
        return Ok(p.into());
    }
    const CANDIDATES: [&str; 5] = [
        "/usr/bin/brave",
        "/usr/bin/brave-browser",
        "/opt/brave-bin/brave",
        "/usr/bin/chromium",
        "/usr/bin/google-chrome-stable",
    ];
    CANDIDATES
        .iter()
        .map(std::path::PathBuf::from)
        .find(|p| p.exists())
        .ok_or_else(|| anyhow!("no chromium-based browser found; set SCOUR_BROWSER=/path/to/brave"))
}

/// One reused browser for the whole process: launching it per request is
/// the difference between ~800ms and ~3s.
pub fn get_browser() -> Result<&'static Mutex<headless_chrome::Browser>> {
    browser()
}
fn browser() -> Result<&'static Mutex<headless_chrome::Browser>> {
    static B: OnceLock<Mutex<headless_chrome::Browser>> = OnceLock::new();
    if let Some(b) = B.get() {
        return Ok(b);
    }
    let opts = headless_chrome::LaunchOptions::default_builder()
        .path(Some(browser_path()?))
        .headless(true)
        .sandbox(false)
        .window_size(Some((1440, 900)))
        // Never touch the user's real Brave profile: a scraper must not read
        // or corrupt their logged-in sessions. headless_chrome defaults to a
        // throwaway temp profile, which is what we want.
        .user_data_dir(None)
        .build()
        .map_err(|e| anyhow!("browser launch opts: {e}"))?;
    let b = headless_chrome::Browser::new(opts).map_err(|e| anyhow!("browser launch: {e}"))?;
    let _ = B.set(Mutex::new(b));
    B.get().ok_or_else(|| anyhow!("browser init race"))
}

fn fetch_chrome_blocking(
    url: &str,
    first_status: u16,
    first_reason: Option<String>,
) -> Result<Fetched> {
    let tab = {
        let b = browser().map_err(|e| anyhow!("{e}"))?;
        let guard = b.lock().map_err(|_| anyhow!("browser mutex poisoned"))?;
        guard.new_tab().map_err(|e| anyhow!("new_tab: {e}"))?
    };
    tab.set_default_timeout(Duration::from_secs(30));
    tab.navigate_to(url).map_err(|e| anyhow!("navigate: {e}"))?;
    tab.wait_until_navigated()
        .map_err(|e| anyhow!("wait nav: {e}"))?;

    // Cloudflare interstitials self-redirect after a few seconds. Poll instead
    // of a fixed sleep so clean pages return immediately.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut html = tab.get_content().unwrap_or_default();
    let mut reason = block_reason(200, &html);
    while reason.is_some() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(700));
        html = tab.get_content().unwrap_or_default();
        reason = block_reason(200, &html);
    }
    let _ = tab.close(true);
    Ok(Fetched {
        blocked: reason.is_some(),
        reason,
        first_reason,
        html,
        tier: Tier::Chrome,
        status: 200,
        escalated: true,
        first_status,
    })
}

/// Fetch `url`, escalating to a real browser only when the cheap path is blocked.
pub async fn fetch(url: &str) -> Result<Fetched> {
    let cheap = fetch_http(url).await;
    match &cheap {
        Ok(f) if !f.blocked => return cheap,
        _ => {}
    }
    let first_status = cheap.as_ref().map(|f| f.status).unwrap_or(0);
    let first_reason = cheap.as_ref().ok().and_then(|f| f.reason.clone());
    let u = url.to_string();
    // headless_chrome is sync; keep it off the async runtime's worker threads.
    match tokio::task::spawn_blocking(move || fetch_chrome_blocking(&u, first_status, first_reason))
        .await?
    {
        Ok(f) => Ok(f),
        // Browser missing/unlaunchable: better to hand back the blocked cheap
        // response (agent sees the challenge page, `blocked: true`) than to
        // fail the call outright.
        Err(e) => cheap.map_err(|_| e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pad(html: &str) -> String {
        // Push past the near-empty guard so we test the interesting layers.
        format!("{html}<!--{}-->", "p".repeat(600))
    }

    #[test]
    fn detects_known_vendors_and_status() {
        assert!(block_reason(403, &"x".repeat(9000)).is_some());
        assert!(block_reason(200, &pad("<html><title>Just a moment...</title></html>")).is_some());
        assert!(
            block_reason(200, &pad("<script src='/cdn-cgi/challenge-platform/h/b/x'></script>"))
                .is_some(),
            "vendor infra path should be caught without any English copy"
        );
    }

    #[test]
    fn catches_unknown_vendor_with_200_and_no_marker() {
        // The gap this fixes: novel protection service, HTTP 200, no phrase we
        // ship, no vendor we know. Structure alone must give it away.
        let wall = format!(
            "<html><body><p>Access verification</p>\
             <script>{}</script></body></html>",
            "var a=1;".repeat(400)
        );
        assert!(block_reason(200, &wall).is_some());
    }

    #[test]
    fn passes_real_pages_including_short_static_ones() {
        let article = format!(
            "<html><body><article>{}</article></body></html>",
            "word ".repeat(400)
        );
        assert!(block_reason(200, &article).is_none());

        // Short, script-free page (example.com shape) must NOT trigger a
        // browser escalation - that was a 0.02s -> 3s regression risk.
        let small = format!(
            "<html><body><h1>Example Domain</h1><p>{}</p></body></html>",
            "This domain is for use in documentation. ".repeat(12)
        );
        assert!(block_reason(200, &small).is_none());

        // Long article that merely quotes an interstitial phrase.
        let quoting = format!(
            "<html><body><article>Cloudflare shows 'just a moment' while checking your browser. {}</article></body></html>",
            "analysis text ".repeat(6000)
        );
        assert!(block_reason(200, &quoting).is_none());
    }

    #[test]
    fn script_and_text_accounting() {
        let (vis, scr) = visible_and_script_len("<p>abc def</p><script>xxxxxxxx</script>");
        assert_eq!(vis, 6, "visible counts non-whitespace text only");
        assert!(scr >= 8, "script bytes counted");
    }
}
