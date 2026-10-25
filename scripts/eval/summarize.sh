#!/usr/bin/env bash
# Aggregate agent-eval results across repeat runs: the results folder plus
# any archive-*/ subfolders you moved earlier runs into. Prints accuracy and
# tokens-per-correct per run, so run-to-run variance is visible before you
# quote any single number.
#
# Usage: ./scripts/eval/summarize.sh [corpus-filter]
# Reads $HEARTHFS_EVAL_RESULTS (default: scripts/eval/results/).

set -euo pipefail
cd "${HEARTHFS_EVAL_RESULTS:-$(dirname "$0")/results}"

FILTER="${1:-}"

find . -name '*.jsonl' | sed 's|.*/||' | sort -u | while read -r name; do
    base="${name%.jsonl}"
    [ -n "$FILTER" ] && case "$base" in *"$FILTER"*) ;; *) continue ;; esac
    echo "== $base"
    find . -name "$name" | sort | while read -r f; do
        jq -s --arg run "$(dirname "$f" | sed 's|^\./||; s|^\.$|current|')" '
          {run: $run,
           n: length,
           correct: (map(select(.verdict=="CORRECT"))|length),
           tokens_per_correct_k: (if (map(select(.verdict=="CORRECT"))|length) > 0
             then ((map(.input_tokens+.output_tokens)|add)/(map(select(.verdict=="CORRECT"))|length)/1000|round)
             else null end),
           avg_turns: ((map(.turns)|add)/length*10|round/10)}
          | "\(.run): \(.correct)/\(.n) correct, \(.tokens_per_correct_k)k tok/correct, \(.avg_turns) turns"' -r "$f"
    done
done
