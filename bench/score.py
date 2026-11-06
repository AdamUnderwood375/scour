#!/usr/bin/env python3
# score.py <arm-dir>    e.g. python3 bench/score.py bench/results/v1/skill
# Answer-quality score per wave (0-100) + total (mean), printed as JSON.
# stdlib only. Ground truth fetched live and cached in-memory for the run.
import json, sys, os, re, urllib.request

UA = 'scourbench/1 (adam)'
SEARCH_ENGINES = ('google.', 'bing.', 'duckduckgo.', 'yandex.', 'baidu.',
                  '/search?', 'search.brave', 'ecosia.')

_gt = {}

def _get_json(url):
    req = urllib.request.Request(url, headers={'User-Agent': UA})
    with urllib.request.urlopen(req, timeout=15) as r:
        return json.load(r)

def ground_truth():
    if _gt:
        return _gt
    for name, crate, repo in (
        ('axum', 'axum', 'tokio-rs/axum'),
        ('actix', 'actix-web', 'actix/actix-web'),
    ):
        ver = stars = None
        try:
            ver = _get_json(f'https://crates.io/api/v1/crates/{crate}')['crate']['max_stable_version']
        except Exception:
            ver = None
        try:
            stars = _get_json(f'https://api.github.com/repos/{repo}')['stargazers_count']
        except Exception:
            stars = None
        _gt[name] = {'version': ver, 'stars': stars}
    return _gt

def read_md(arm, w):
    p = os.path.join(arm, f'w{w}.md')
    return open(p, encoding='utf-8', errors='replace').read() if os.path.exists(p) else ''

def urls(text):
    found = re.findall(r'https?://[^\s<>()\]\[»"\'`]+', text)
    out = []
    for u in found:
        u = u.rstrip('.,);:')
        if u not in out:
            out.append(u)
    return out

def non_search_urls(text):
    return [u for u in urls(text) if not any(s in u.lower() for s in SEARCH_ENGINES)]

NUM_RE = re.compile(r'~?\s*(\d[\d,]*(?:\.\d+)?)\s*([kK])?')

def numbers(text):
    vals = []
    for m in NUM_RE.finditer(text):
        raw = m.group(1).replace(',', '')
        try:
            v = float(raw)
        except ValueError:
            continue
        if m.group(2):
            v *= 1000
        vals.append(v)
    return vals

def stars_present(text, target):
    if target is None:
        return 0.0
    lo, hi = target * 0.97, target * 1.03
    return 1.0 if any(lo <= v <= hi for v in numbers(text)) else 0.0

def url_live(u):
    # returns 'ok' | 'bad' | 'unverifiable'
    try:
        req = urllib.request.Request(u, headers={'User-Agent': UA})
        with urllib.request.urlopen(req, timeout=10) as r:
            return 'ok' if r.status < 400 else 'bad'
    except urllib.error.HTTPError as e:
        if e.code in (403, 429):
            return 'unverifiable'
        return 'bad'
    except Exception:
        return 'bad'

def score_w1(arm, gt):
    t = read_md(arm, 1)
    items = {
        'axum_version': 1.0 if (gt['axum']['version'] and gt['axum']['version'] in t) else 0.0,
        'actix_version': 1.0 if (gt['actix']['version'] and gt['actix']['version'] in t) else 0.0,
        'axum_stars': stars_present(t, gt['axum']['stars']),
        'actix_stars': stars_present(t, gt['actix']['stars']),
        'three_urls': 1.0 if len(urls(t)) >= 3 else 0.0,
    }
    score = round(100 * sum(items.values()) / len(items), 1)
    return score, items

def score_w2(arm, gt):
    t = read_md(arm, 2)
    us = non_search_urls(t)
    has3 = 1.0 if len(us) >= 3 else 0.0
    # liveness over up to 3 urls
    live_frac = 0.0
    live_detail = {}
    check = us[:3]
    if check:
        tot = 0.0
        for u in check:
            st = url_live(u)
            live_detail[u] = st
            tot += {'ok': 1.0, 'unverifiable': 0.5, 'bad': 0.0}[st]
        live_frac = tot / len(check)
    year = 1.0 if ('2025' in t or '2026' in t) else 0.0
    tl = t.lower()
    tradeoff = 1.0 if re.search(r'trade-?offs?', tl) else 0.0
    items = {'three_nonsearch_urls': has3, 'urls_live': round(live_frac, 2),
             'mentions_year': year, 'tradeoffs': tradeoff}
    score = round(100 * sum(items.values()) / len(items), 1)
    return score, items, live_detail

def score_w3(arm, gt):
    t = read_md(arm, 3)
    rust_blocks = re.findall(r'```rust\b(.*?)```', t, re.S)
    has_rust = 1.0 if rust_blocks else 0.0
    api = 1.0 if ('from_fn' in t or 'middleware::' in t or 'Layer' in t) else 0.0
    doc_url = 1.0 if ('docs.rs/axum' in t or 'docs.rs/crate/axum' in t
                      or 'github.com/tokio-rs/axum' in t) else 0.0
    code = '\n'.join(rust_blocks)
    working = 1.0 if ('axum::' in code and ('fn main' in code or '#[tokio::main]' in code)) else 0.0
    items = {'rust_block': has_rust, 'middleware_api': api,
             'docs_url': doc_url, 'working_example': working}
    score = round(100 * sum(items.values()) / len(items), 1)
    return score, items

def main():
    arm = sys.argv[1].rstrip('/')
    gt = ground_truth()
    s1, i1 = score_w1(arm, gt)
    s2, i2, live = score_w2(arm, gt)
    s3, i3 = score_w3(arm, gt)
    total = round((s1 + s2 + s3) / 3, 1)
    print(json.dumps({
        'arm': arm,
        'ground_truth': gt,
        'w1': {'score': s1, 'items': i1},
        'w2': {'score': s2, 'items': i2, 'url_liveness': live},
        'w3': {'score': s3, 'items': i3},
        'quality_total': total,
    }, indent=1))

if __name__ == '__main__':
    main()
