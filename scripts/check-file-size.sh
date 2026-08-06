#!/usr/bin/env bash
#
# Fails when a source file grows past LIMIT lines.
#
# This is a blunt instrument on purpose. The clippy lints that claim to measure
# the same thing do not: `too_many_lines` is pedantic, defaults to 100, and
# counts test functions under `--all-targets`, so adopting it means writing a
# dozen `#[allow]`s for functions that are long by design; `cognitive_complexity`
# is a nursery lint whose heuristic drifts between toolchains, which in a repo
# with a pinned MSRV job means a Rust upgrade can redden CI for no change at
# all. A line count has no heuristic to argue with and is auditable at a glance.
#
# What it cannot do: a line cap is gameable, and a badly chosen cap pushes
# people to split files along the wrong seams. Two things keep that honest —
# it caps *files* rather than functions, so the pressure is to find a real
# module boundary rather than to shred a function; and every exception is a
# line in `file-size-allowlist.txt`, reviewed in a diff, rather than an
# invisible attribute in the source.
#
# The allowlist is self-cleaning: an entry that has dropped under the limit is
# an error too, so the list shrinks as the work lands instead of rotting.

set -euo pipefail

LIMIT=${LIMIT:-1500}
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ALLOWLIST="$ROOT/scripts/file-size-allowlist.txt"

allowed() {
    grep -qxF "$1" "$ALLOWLIST"
}

over=()
while IFS= read -r -d '' file; do
    rel="${file#"$ROOT"/}"
    lines=$(wc -l <"$file")
    if [ "$lines" -gt "$LIMIT" ]; then
        allowed "$rel" || over+=("$rel ($lines lines)")
    fi
done < <(find "$ROOT/crates" -path '*/src/*' -name '*.rs' -print0)

stale=()
while IFS= read -r rel; do
    case "$rel" in '' | '#'*) continue ;; esac
    if [ ! -f "$ROOT/$rel" ]; then
        stale+=("$rel (no such file)")
        continue
    fi
    lines=$(wc -l <"$ROOT/$rel")
    if [ "$lines" -le "$LIMIT" ]; then
        stale+=("$rel ($lines lines — now under the limit)")
    fi
done <"$ALLOWLIST"

status=0
if [ ${#over[@]} -gt 0 ]; then
    printf 'Files over %s lines and not allowlisted:\n' "$LIMIT" >&2
    printf '  %s\n' "${over[@]}" >&2
    printf '\nSplit the file, or add it to scripts/file-size-allowlist.txt with\n' >&2
    printf 'a line saying why it earns the exception.\n' >&2
    status=1
fi
if [ ${#stale[@]} -gt 0 ]; then
    printf 'Stale entries in scripts/file-size-allowlist.txt:\n' >&2
    printf '  %s\n' "${stale[@]}" >&2
    printf '\nDrop them — the allowlist is a backlog, not a record.\n' >&2
    status=1
fi

[ "$status" -eq 0 ] && printf 'No source file over %s lines outside the allowlist.\n' "$LIMIT"
exit "$status"
