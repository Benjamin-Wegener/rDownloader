#!/usr/bin/env bash
# Orchestrierung für beliebig viele Agents (siehe AGENTS.md). Läuft vom Hauptordner aus.
#   scripts/agent.sh next          nächsten startbaren Schritt wählen, sperren, Worktree anlegen
#   scripts/agent.sh finish <id>   rebase + Fast-Forward-Merge nach master, Aufräumen, als erledigt markieren
#   scripts/agent.sh abort <id>    Schritt aufgeben (Sperre + Worktree weg, nicht erledigt)
#   scripts/agent.sh status        Queue mit Zuständen
set -euo pipefail
main=$(git worktree list --porcelain | awk '/^worktree /{print $2; exit}')
state="$(git -C "$main" rev-parse --git-common-dir)"; case "$state" in /*) ;; *) state="$main/$state";; esac
state="$state/orch"; mkdir -p "$state/claims" "$state/done" "$state/areas"
queue="$main/QUEUE.tsv"
rows() { grep -v '^#' "$queue" | grep -v '^[[:space:]]*$'; }
is_done() { [ -e "$state/done/$1" ]; }

startable() { # gibt "id<TAB>bereich<TAB>titel" der ersten startbaren Zeile aus
  declare -A blocked=()
  while IFS=$'\t' read -r id area title needs; do
    is_done "$id" && continue
    if [ -n "${blocked[$area]:-}" ]; then continue; fi
    blocked[$area]=1                       # Bereich strikt sequenziell: nur der erste offene Schritt zählt
    [ -e "$state/areas/$area" ] && continue
    ok=1; if [ "$needs" != "-" ]; then IFS=, read -ra n <<<"$needs"; for d in "${n[@]}"; do is_done "$d" || ok=0; done; fi
    [ $ok = 1 ] && { printf '%s\t%s\t%s\n' "$id" "$area" "$title"; return 0; }
  done < <(rows)
  return 1
}

case "${1:-}" in
  next)
    git -C "$main" diff --quiet && git -C "$main" diff --cached --quiet || { echo "Hauptordner hat uncommittete Änderungen – dort nie direkt arbeiten."; exit 1; }
    for try in 1 2 3 4 5; do
      line=$(startable) || { echo "NICHTS STARTBAR. Alles erledigt oder blockiert/vergeben. Siehe: scripts/agent.sh status"; exit 3; }
      IFS=$'\t' read -r id area title <<<"$line"
      if mkdir "$state/areas/$area" 2>/dev/null; then
        mkdir "$state/claims/$id"; echo "$id" > "$state/areas/$area/step"
        wt="$main/../rdl-$id"
        git -C "$main" worktree add -q -b "step/$id" "$wt" master
        echo "SCHRITT: $id ($area): $title"; echo "ARBEITSORDNER: $(cd "$wt" && pwd)"
        echo "Wechsle dorthin (cd), arbeite NUR dort. Danach: $main/scripts/agent.sh finish $id"; exit 0
      fi
    done; echo "Wettlauf, bitte erneut versuchen"; exit 1 ;;
  finish)
    id=$2; wt="$main/../rdl-$id"; area=$(rows | awk -F'\t' -v i="$id" '$1==i{print $2}')
    mkdir "$state/merge" 2>/dev/null || { echo "Merge-Lock belegt, in einer Minute erneut"; exit 1; }
    trap 'rm -rf "$state/merge"' EXIT
    git -C "$wt" diff --quiet && git -C "$wt" diff --cached --quiet || { echo "Uncommittete Änderungen in $wt – erst committen"; exit 1; }
    git -C "$wt" rebase master
    git -C "$main" merge --ff-only "step/$id"
    git -C "$main" worktree remove --force "$wt"; git -C "$main" branch -q -d "step/$id"
    touch "$state/done/$id"; rm -rf "$state/claims/$id" "$state/areas/$area"; echo "erledigt: $id" ;;
  abort)
    id=$2; area=$(rows | awk -F'\t' -v i="$id" '$1==i{print $2}')
    git -C "$main" worktree remove --force "$main/../rdl-$id" 2>/dev/null || true; git -C "$main" branch -q -D "step/$id" 2>/dev/null || true
    rm -rf "$state/claims/$id" "$state/areas/$area"; echo "aufgegeben: $id" ;;
  status)
    while IFS=$'\t' read -r id area title needs; do
      s=offen; is_done "$id" && s=ERLEDIGT; [ -e "$state/claims/$id" ] && s="IN ARBEIT"
      printf '%-10s %-10s %s\n' "$id" "$s" "$title"
    done < <(rows) ;;
  *) sed -n '2,6p' "$0"; exit 2 ;;
esac
