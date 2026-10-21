#!/usr/bin/env bash
# Hilfsskript für die Agent-Orchestrierung (siehe AGENTS.md).
#   scripts/agent.sh lane                 Lane-Bereich des aktuellen Worktrees
#   scripts/agent.sh claim <name>         Schritt atomar sperren (Exit 1 = schon vergeben)
#   scripts/agent.sh release <name>       Sperre freigeben
#   scripts/agent.sh claims               Aktive Sperren anzeigen
#   scripts/agent.sh merge                Lane-Branch nach master mergen (mit Merge-Lock)
#   scripts/agent.sh add <bereich> [n]    Neuen Worktree anlegen, z. B. "add core 2"
set -euo pipefail
common=$(git rev-parse --git-common-dir)
claims="$common/claims"; mkdir -p "$claims"
main=$(git worktree list --porcelain | awk '/^worktree /{print $2; exit}')
branch=$(git branch --show-current)

case "${1:-}" in
  lane)    b=${branch#lane/}; echo "${b%-[0-9]*}" ;;
  claim)   mkdir "$claims/$2" 2>/dev/null && echo "geclaimt: $2 ($branch)" && echo "$branch" > "$claims/$2/owner" \
             || { echo "bereits vergeben an: $(cat "$claims/$2/owner" 2>/dev/null)"; exit 1; } ;;
  release) rm -rf "$claims/$2" ;;
  claims)  for d in "$claims"/*/; do [ -d "$d" ] && echo "$(basename "$d") -> $(cat "$d/owner" 2>/dev/null)"; done || true ;;
  merge)
    mkdir "$claims/_merge" 2>/dev/null || { echo "Merge-Lock belegt, später erneut"; exit 1; }
    trap 'rm -rf "$claims/_merge"' EXIT
    git rebase master
    git -C "$main" merge --ff-only "$branch" ;;
  add)
    n=${3:-}; name="$2${n:+-$n}"
    git worktree add -b "lane/$name" "$main/../rdl-$name" master ;;
  *) sed -n '2,8p' "$0"; exit 2 ;;
esac
