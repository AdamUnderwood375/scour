#!/usr/bin/env bash
# Scour benchmark: drives the `pi` coding agent through 3 research waves.
# Usage: run.sh <label> [arm]     arm = skill (default) | mcp
#
# Config via env (sensible defaults):
#   SCOUR_BENCH_OUT    output root            (default: bench/results)
#   SCOUR_SKILL_DIR    scour skill dir        (default: <repo>/skills/scour)
#   SCOUR_MCP_EXT      pi MCP extension path  (arm=mcp only; required then)
#   SCOUR_BENCH_MODEL  model id               (default: anthropic/claude-haiku-5-5)
#   SCOUR_BIN          scour binary           (default: <repo>/target/release/scour)
set -u

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(dirname "$SCRIPT_DIR")

LABEL=${1:?usage: run.sh <label> [arm]}
ARM=${2:-skill}

OUT=${SCOUR_BENCH_OUT:-bench/results}
SKILL_DIR=${SCOUR_SKILL_DIR:-$REPO/skills/scour}
MODEL=${SCOUR_BENCH_MODEL:-anthropic/claude-haiku-5-5}
SCOUR=${SCOUR_BIN:-$REPO/target/release/scour}

command -v pi >/dev/null 2>&1 || { echo "error: 'pi' CLI not found on PATH" >&2; exit 1; }

if [ ! -x "$SCOUR" ]; then
  echo "error: scour binary not found/executable at $SCOUR (run: cargo build --release, or set SCOUR_BIN)" >&2
  exit 1
fi
# So the skill's `command -v scour` resolves to this build.
export PATH="$(dirname "$SCOUR"):$PATH"

if [ "$ARM" = "mcp" ] && [ -z "${SCOUR_MCP_EXT:-}" ]; then
  echo "error: arm 'mcp' requires SCOUR_MCP_EXT to point at a pi MCP extension" >&2
  exit 1
fi

B=$OUT/$LABEL

# Refuse to reuse an arm dir: its pi session id would resume the old session.
if [ -e "$B/$ARM" ]; then
  echo "error: $B/$ARM already exists; choose a new label" >&2
  exit 1
fi

clear_cache() {
  rm -f "$HOME/.cache/scour/cache.json"
}

wave_prompt() {
  local arm=$1 w=$2
  case $w in
    1) cat <<EOF
Research wave 1. Topic: the Rust web frameworks Axum and Actix Web.
Get real current facts from the web — do not answer from memory. For each framework: latest version number, GitHub star count, and one notable feature. Use at least 3 distinct web pages.
Write the result (short table + URLs) to $B/$arm/w1.md.
EOF
    ;;
    2) cat <<EOF
Research wave 2. Find 3 recent (2025-2026) articles or discussions comparing Axum and Actix Web. Summarise the consensus and 2 concrete trade-offs, with URLs.
Append to $B/$arm/w2.md. Use the web, not memory.
EOF
    ;;
    3) cat <<EOF
Research wave 3. From the official Axum documentation, find the current middleware API for the latest version and include a working code example. Fetch the docs pages you need, including linked sub-pages.
Write $B/$arm/w3.md with the code example and URLs.
EOF
    ;;
  esac
}

run_arm() {
  local arm=$1
  mkdir -p "$B/$arm"
  local sid="$LABEL-$arm-session"
  for w in 1 2 3; do
    clear_cache
    local p; p=$(wave_prompt "$arm" "$w")
    local t0 t1
    t0=$(date +%s%3N)
    if [ "$arm" = "skill" ]; then
      timeout 900 pi -p --mode json --model "$MODEL" --thinking off \
        --no-skills --no-extensions --no-context-files --skill "$SKILL_DIR" --session-id "$sid" \
        "$p" > "$B/$arm/w$w.json" 2> "$B/$arm/w$w.err"
    else
      timeout 900 pi -p --mode json --model "$MODEL" --thinking off \
        --no-skills --no-extensions --no-context-files -e "$SCOUR_MCP_EXT" --session-id "$sid" \
        "$p" > "$B/$arm/w$w.json" 2> "$B/$arm/w$w.err"
    fi
    local rc=$?
    t1=$(date +%s%3N)
    echo "$w $((t1-t0)) $rc" >> "$B/$arm/timing.txt"
  done
}

run_arm "$ARM"
