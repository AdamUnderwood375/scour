#!/usr/bin/env python3
# compare.py <arm-dir>...    e.g. python3 bench/compare.py bench/results/v1/skill
# One aligned row per arm: calls, total_tokens, result_chars, cost_usd, wall_s, quality.
# Adds a 'mean' row when 2+ dirs are given. Paths are absolute or relative to cwd.
import json, sys, subprocess, os

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import score  # reuse in-memory ground-truth cache across arms

def parse_agg(arm):
    out = subprocess.run([sys.executable, os.path.join(HERE, 'parse.py'), arm],
                         capture_output=True, text=True)
    if out.returncode != 0:
        return {}
    return json.loads(out.stdout).get('agg', {})

def quality(arm):
    gt = score.ground_truth()
    s1, _ = score.score_w1(arm, gt)
    s2, _, _ = score.score_w2(arm, gt)
    s3, _ = score.score_w3(arm, gt)
    return round((s1 + s2 + s3) / 3, 1)

def main():
    arms = sys.argv[1:]
    if not arms:
        print('usage: compare.py <arm-dir>...'); return
    cols = ['arm', 'calls', 'total_tokens', 'result_chars', 'cost_usd', 'wall_s', 'quality']
    rows = []
    for arm in arms:
        arm = arm.rstrip('/')
        a = parse_agg(arm)
        rows.append([
            arm,
            a.get('calls', 0),
            a.get('total', 0),
            a.get('result_chars', 0),
            a.get('total_cost_usd', 0),
            a.get('wall_s', 0),
            quality(arm),
        ])
    if len(rows) >= 2:
        n = len(rows)
        mean = ['mean']
        for i in range(1, len(cols)):
            mean.append(round(sum(r[i] for r in rows) / n, 1))
        rows.append(mean)
    widths = [max(len(str(c)), max((len(str(r[i])) for r in rows), default=0))
              for i, c in enumerate(cols)]
    def fmt(vals):
        return '  '.join(str(v).rjust(widths[i]) for i, v in enumerate(vals))
    print(fmt(cols))
    for r in rows:
        print(fmt(r))

if __name__ == '__main__':
    main()
