# scour benchmark harness

Reproducible benchmark that measures how `scour` helps an LLM coding agent
(the `pi` CLI) do real web research, versus an alternative tool arm.

## The task — 3 research waves (one persistent pi session per arm)

1. **w1** — latest version, GitHub stars, one feature for Axum & Actix Web
   (≥3 distinct web pages), written as a table + URLs.
2. **w2** — 3 recent (2025-2026) Axum vs Actix comparisons: consensus + 2
   trade-offs, with URLs.
3. **w3** — current Axum middleware API from official docs + a working code
   example, with URLs.

## What's measured

`parse.py` reads pi's JSON logs per wave and aggregates:
`calls` (assistant turns), `total_tokens`, `result_chars` (tool output size),
`cost_usd`, `wall_s`. `compare.py` prints one aligned row per arm (+ a `mean`
row for 2+ arms).

## How quality is scored (`score.py`, 0–100 mean of 3 waves, live truth)

- **w1**: Axum version match, Actix version match, Axum stars (±3%),
  Actix stars (±3%), ≥3 URLs. Versions/stars fetched live from
  crates.io + GitHub.
- **w2**: ≥3 non-search-engine URLs, URL liveness (HTTP check of up to 3),
  mentions 2025/2026, mentions trade-offs.
- **w3**: has a ```rust block, uses the middleware API
  (`from_fn`/`middleware::`/`Layer`), cites an Axum docs/GitHub URL,
  example looks runnable (`axum::` + `fn main`/`#[tokio::main]`).

## Requirements

- `pi` CLI on PATH + a model provider key (default model
  `anthropic/claude-haiku-5-5`, override with `SCOUR_BENCH_MODEL`).
- Linux (`run.sh` uses GNU `date` and `timeout`).
- A release `scour` build (`cargo build --release`).
- For the `mcp` arm: a pi MCP extension via `SCOUR_MCP_EXT`.

## Run

```sh
cargo build --release
bench/run.sh v1                       # skill arm -> bench/results/v1/skill
python3 bench/compare.py bench/results/v1/skill
```

Env knobs: `SCOUR_BENCH_OUT`, `SCOUR_SKILL_DIR`, `SCOUR_BIN`,
`SCOUR_BENCH_MODEL`, `SCOUR_MCP_EXT` (see `run.sh`).

## Caveats

Single task, single model. Results vary with live web + search-engine
availability. Each `run.sh` costs real model spend. **Run 3× and use the
`mean` row.**
