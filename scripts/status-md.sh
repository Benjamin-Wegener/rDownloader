#!/usr/bin/env bash
# Regenerates STATUS.md (project root, plain text so it works with `tail`) from the queue state and the roadmap checkboxes.
# Run from the main folder; commit the result via a step worktree (see AGENTS.md).
set -euo pipefail
main=$(git worktree list --porcelain | awk '/^worktree /{print $2; exit}')
out="${1:-STATUS.md}"
tmp=$(mktemp); trap 'rm -f "$tmp"' EXIT
"$main/scripts/agent.sh" status > "$tmp"
python3 - "$main/ROADMAP.md" "$out" "$tmp" <<'PY'
import re, sys, datetime, subprocess
roadmap, out, status_file = sys.argv[1:4]
rows = []
for line in open(status_file, encoding='utf-8'):
    m = re.match(r'(\S+)\s+(ERLEDIGT|IN ARBEIT|offen)\s+(.*)', line.rstrip())
    if m:
        rows.append(m.groups())
text = open(roadmap, encoding='utf-8').read()
done_cb, open_cb = len(re.findall(r'\[x\]', text)), len(re.findall(r'\[ \]', text))
lab = {'ERLEDIGT': 'done', 'IN ARBEIT': 'in progress', 'offen': 'open'}
mark = {'ERLEDIGT': '[x]', 'IN ARBEIT': '[~]', 'offen': '[ ]'}
n = len(rows); nd = sum(r[1] == 'ERLEDIGT' for r in rows); nw = sum(r[1] == 'IN ARBEIT' for r in rows)
areas = {}
for i, s, t in rows:
    areas.setdefault(i.rsplit('-', 1)[0], []).append((i, s, t))
try:
    sha = subprocess.check_output(['git', '-C', roadmap.rsplit('/', 1)[0], 'rev-parse', '--short', 'HEAD'], text=True).strip()
except Exception:
    sha = '?'
pct = round(100 * nd / n) if n else 0
bar = '#' * (pct // 5) + '-' * (20 - pct // 5)
lines = ['# rdownloader status', '',
         f'Generated {datetime.date.today().isoformat()} at commit {sha} - regenerate with `scripts/status-md.sh`', '',
         f'`[{bar}]` **{nd}/{n}** steps done ({pct}%), {nw} in progress, {n-nd-nw} open',
         f'Roadmap items: {done_cb} checked, {open_cb} open', '']
for a, items in areas.items():
    d = sum(s == 'ERLEDIGT' for _, s, _ in items)
    lines += [f'## {a} ({d}/{len(items)})', '']
    lines += [f'- {mark[s]} `{i}` {t}' + (' - **in progress**' if s == 'IN ARBEIT' else '') for i, s, t in items]
    lines.append('')
open(out, 'w', encoding='utf-8').write('\n'.join(lines))
PY
echo "wrote $out"
