#!/usr/bin/env bash
# Regenerates STATUS.html (project root) from the queue state and the roadmap checkboxes.
# Run from the main folder; commit the result via a step worktree (see AGENTS.md).
set -euo pipefail
main=$(git worktree list --porcelain | awk '/^worktree /{print $2; exit}')
out="${1:-STATUS.html}"
tmp=$(mktemp); trap 'rm -f "$tmp"' EXIT
"$main/scripts/agent.sh" status > "$tmp"
python3 - "$main/ROADMAP.md" "$out" "$tmp" <<'PY'
import html, re, sys, datetime, subprocess
roadmap, out, status_file = sys.argv[1:4]
rows = []
for line in open(status_file, encoding='utf-8'):
    m = re.match(r'(\S+)\s+(ERLEDIGT|IN ARBEIT|offen)\s+(.*)', line.rstrip())
    if m:
        rows.append(m.groups())
text = open(roadmap, encoding='utf-8').read()
done_cb, open_cb = len(re.findall(r'\[x\]', text)), len(re.findall(r'\[ \]', text))
cls = {'ERLEDIGT': 'done', 'IN ARBEIT': 'wip', 'offen': 'todo'}
lab = {'ERLEDIGT': 'Done', 'IN ARBEIT': 'In progress', 'offen': 'Open'}
n = len(rows); nd = sum(r[1] == 'ERLEDIGT' for r in rows); nw = sum(r[1] == 'IN ARBEIT' for r in rows)
areas = {}
for i, s, t in rows:
    areas.setdefault(i.rsplit('-', 1)[0], []).append((i, s, t))
try:
    sha = subprocess.check_output(['git', '-C', roadmap.rsplit('/', 1)[0], 'rev-parse', '--short', 'HEAD'], text=True).strip()
except Exception:
    sha = '?'
cards = ''
for a, items in areas.items():
    d = sum(s == 'ERLEDIGT' for _, s, _ in items)
    lis = ''.join(f'<li class="{cls[s]}"><span class="id">{html.escape(i)}</span><span class="badge">{lab[s]}</span>{html.escape(t)}</li>' for i, s, t in items)
    cards += f'<section><h2>{html.escape(a)} <small>{d}/{len(items)}</small></h2><ul>{lis}</ul></section>'
pct = round(100 * nd / n) if n else 0
open(out, 'w', encoding='utf-8').write(f'''<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>rdownloader status</title>
<style>
:root{{--bg:#fff;--fg:#1c1c1e;--mut:#6b6b70;--card:#f4f4f6;--done:#1a7f37;--wip:#b35900;--todo:#6b6b70;--bar:#e0e0e4}}
@media(prefers-color-scheme:dark){{:root{{--bg:#121214;--fg:#ececf0;--mut:#9a9aa2;--card:#1c1c20;--done:#4ac26b;--wip:#f0a040;--todo:#9a9aa2;--bar:#33333a}}}}
body{{margin:0;padding:24px 16px;background:var(--bg);color:var(--fg);font:15px/1.5 system-ui,sans-serif}}
main{{max-width:960px;margin:0 auto}}h1{{margin:0 0 4px}}p.sub{{color:var(--mut);margin:0 0 20px}}
.bar{{height:10px;border-radius:5px;background:var(--bar);overflow:hidden}}.bar i{{display:block;height:100%;width:{pct}%;background:var(--done)}}
.stats{{display:flex;flex-wrap:wrap;gap:16px;margin:12px 0 24px;color:var(--mut)}}.stats b{{color:var(--fg)}}
.grid{{display:grid;grid-template-columns:repeat(auto-fit,minmax(300px,1fr));gap:16px}}
section{{background:var(--card);border-radius:10px;padding:14px 16px}}h2{{margin:0 0 8px;font-size:16px;text-transform:uppercase;letter-spacing:.04em}}h2 small{{color:var(--mut);font-weight:400}}
ul{{list-style:none;margin:0;padding:0}}li{{padding:6px 0;border-top:1px solid var(--bar)}}li:first-child{{border-top:0}}
.id{{font:600 12px ui-monospace,monospace;margin-right:8px}}.badge{{font-size:11px;padding:1px 7px;border-radius:9px;margin-right:8px;border:1px solid currentColor}}
.done .badge{{color:var(--done)}}.wip .badge{{color:var(--wip)}}.todo .badge{{color:var(--todo)}}.todo{{color:var(--mut)}}
</style></head><body><main>
<h1>rdownloader status</h1>
<p class="sub">Generated {datetime.date.today().isoformat()} at commit {html.escape(sha)} · regenerate with <code>scripts/status-html.sh</code></p>
<div class="bar" role="progressbar" aria-valuenow="{pct}" aria-valuemin="0" aria-valuemax="100"><i></i></div>
<div class="stats"><span><b>{nd}</b> / {n} steps done ({pct}%)</span><span><b>{nw}</b> in progress</span><span><b>{n-nd-nw}</b> open</span><span>Roadmap items: <b>{done_cb}</b> checked, <b>{open_cb}</b> open</span></div>
<div class="grid">{cards}</div>
</main></body></html>''')
PY
echo "wrote $out"
