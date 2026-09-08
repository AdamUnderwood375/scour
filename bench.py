#!/usr/bin/env python3
"""Benchmark stdio MCP servers on identical URLs, N trials each.

Times the tools/call round trip only. Startup + handshake is reported
separately since it is paid once per session, not per scrape.
Caching is disabled on both sides so every trial is a real fetch.
"""
import json, statistics, subprocess, sys, time

URLS = [
    "https://example.com",
    "https://react.dev/learn",
    "https://en.wikipedia.org/wiki/Rust_(programming_language)",
    "https://news.ycombinator.com",
]
TRIALS = 3

SERVERS = {
    "scour": (["/home/adam-underwood/scour/target/release/scour"], "scrape",
              lambda u: {"url": u}),
    "hound": (["/home/adam-underwood/.local/bin/hound"], "mcp_smart_fetch",
              lambda u: {"url": u, "cache_ttl": 0}),
}


def rpc(p, o):
    p.stdin.write(json.dumps(o) + "\n")
    p.stdin.flush()


def read_id(p, want):
    while True:
        line = p.stdout.readline()
        if not line:
            return None
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        if msg.get("id") == want:
            return msg


def bench(name):
    cmd, tool, args_for = SERVERS[name]
    p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.DEVNULL, text=True, bufsize=1)
    t0 = time.perf_counter()
    rpc(p, {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2024-11-05", "capabilities": {},
        "clientInfo": {"name": "bench", "version": "1"}}})
    if read_id(p, 1) is None:
        return {}
    startup = time.perf_counter() - t0
    rpc(p, {"jsonrpc": "2.0", "method": "notifications/initialized"})

    out = {"_startup": startup}
    rid = 2
    for url in URLS:
        times, size = [], 0
        for _ in range(TRIALS):
            t = time.perf_counter()
            rpc(p, {"jsonrpc": "2.0", "id": rid, "method": "tools/call",
                    "params": {"name": tool, "arguments": args_for(url)}})
            resp = read_id(p, rid)
            times.append(time.perf_counter() - t)
            rid += 1
            if resp and "result" in resp and not resp["result"].get("isError"):
                size = len("".join(c.get("text", "")
                                   for c in resp["result"].get("content", [])))
            else:
                size = -1
        out[url] = (statistics.median(times), size)
    p.stdin.close()
    p.terminate()
    return out


def main():
    res = {n: bench(n) for n in SERVERS}
    print(f"\nmedian of {TRIALS} trials, cache off\n")
    print(f"{'URL':<46} {'scour':>19} {'hound':>19}")
    print("-" * 86)
    print(f"{'startup + handshake':<46} "
          f"{res['scour']['_startup']*1000:>16.0f}ms {res['hound']['_startup']*1000:>16.0f}ms")
    for u in URLS:
        row = f"{u[:44]:<46}"
        for n in ("scour", "hound"):
            dt, sz = res[n].get(u, (0, -1))
            row += f" {dt:>7.2f}s {sz:>8,}c" if sz >= 0 else f"{'FAILED':>19}"
        print(row)
    print("\nc = chars of tool payload returned to the agent")


if __name__ == "__main__":
    main()
