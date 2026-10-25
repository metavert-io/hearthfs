#!/usr/bin/env bash
# Agent evaluation — the end-to-end tier of the HearthFS retrieval eval
# (docs/retrieval-eval.md).
#
# Runs a headless Claude Code agent over one corpus in one of two conditions
# and scores its answers with an LLM judge:
#   raw       — the agent works over the raw corpus folder (grep/cat baseline)
#   hearthfs  — the agent works over a HearthFS mount and is told about magic
#               paths
#
# The headline metric is tokens spent per correct answer.
#
# Usage:
#   ./scripts/eval/agent_eval.sh <root> <corpus> <raw|hearthfs> [question-id]
#
# Requirements: claude CLI, jq. For the hearthfs condition the corpus must
# be mounted first (run_stage.sh does this for you):
#   hearthfs mount <root>/<corpus>/data /tmp/hearthfs-eval-mount
#
# Environment: HEARTHFS_EVAL_MODEL, HEARTHFS_EVAL_JUDGE_MODEL,
# HEARTHFS_EVAL_MAX_TURNS, HEARTHFS_EVAL_MOUNT, HEARTHFS_EVAL_RESULTS.
#
# Results append to $HEARTHFS_EVAL_RESULTS/<corpus>-<condition>.jsonl
# (default: scripts/eval/results/, which git ignores).

set -euo pipefail

ROOT="${1:?usage: agent_eval.sh <root> <corpus> <raw|hearthfs> [question-id]}"
CORPUS="${2:?missing corpus (a folder under <root> holding question.json)}"
CONDITION="${3:?missing condition: raw | hearthfs}"
ONLY_QID="${4:-}"

CORPUS_DIR="$ROOT/$CORPUS"
QUESTIONS="$CORPUS_DIR/question.json"
RESULTS_DIR="${HEARTHFS_EVAL_RESULTS:-$(cd "$(dirname "$0")" && pwd)/results}"
mkdir -p "$RESULTS_DIR"
OUT="$RESULTS_DIR/$CORPUS-$CONDITION.jsonl"

MODEL="${HEARTHFS_EVAL_MODEL:-claude-sonnet-4-6}"
JUDGE_MODEL="${HEARTHFS_EVAL_JUDGE_MODEL:-claude-sonnet-4-6}"
MAX_TURNS="${HEARTHFS_EVAL_MAX_TURNS:-25}"

case "$CONDITION" in
    raw)
        WORKDIR="$CORPUS_DIR/data"
        EXTRA_SYSTEM="Answer the user's question by searching and reading the files in the current directory using grep, find, ls, and file reads."
        ;;
    hearthfs)
        WORKDIR="${HEARTHFS_EVAL_MOUNT:-/tmp/hearthfs-eval-mount}"
        # Substring match: /tmp is a symlink, so the mount table lists the
        # resolved /private/tmp path.
        if ! mount | grep -qF "$WORKDIR"; then
            echo "error: $WORKDIR is not mounted. Run: hearthfs mount $CORPUS_DIR/data $WORKDIR" >&2
            exit 1
        fi
        EXTRA_SYSTEM="Answer the user's question by searching and reading the files in the current directory. This directory is a HearthFS semantic filesystem mount with magic paths — prefer these over grep, they are far cheaper and rank by meaning:
- cat '@search/<query>' — hybrid semantic + full-text search over the whole tree, returns ranked file paths with snippets. Start here.
- cat '<file>@outline' — structural outline of a file with token counts per section.
- cat '<file>@section/<name>' — extract one named section instead of reading the whole file.
- cat '<file>@tokens' — token count for a file.
Quote paths containing @ or spaces."
        ;;
    *)
        echo "error: condition must be raw or hearthfs" >&2
        exit 1
        ;;
esac

N_QUESTIONS=$(jq length "$QUESTIONS")
echo "== $CORPUS / $CONDITION — $N_QUESTIONS questions, model $MODEL, cwd $WORKDIR"

for i in $(seq 0 $((N_QUESTIONS - 1))); do
    QID=$(jq -r ".[$i].id" "$QUESTIONS")
    if [ -n "$ONLY_QID" ] && [ "$QID" != "$ONLY_QID" ]; then
        continue
    fi
    if [ -z "$ONLY_QID" ] && [ -f "$OUT" ] && jq -e -s --arg qid "$QID" 'map(select(.qid == $qid)) | length > 0' "$OUT" >/dev/null 2>&1; then
        echo "-- $QID: already recorded, skipping"
        continue
    fi

    PROMPT=$(jq -r ".[$i].prompt" "$QUESTIONS")
    GOLD=$(jq -r ".[$i].gold_answer" "$QUESTIONS")
    FAMILY=$(jq -r ".[$i].family" "$QUESTIONS")

    echo "-- $QID ($FAMILY): $PROMPT"
    START=$(date +%s)
    AGENT_JSON=$(cd "$WORKDIR" && claude -p "$PROMPT" \
        --model "$MODEL" \
        --max-turns "$MAX_TURNS" \
        --allowedTools "Bash,Read,Grep,Glob" \
        --append-system-prompt "$EXTRA_SYSTEM Give a direct, concise final answer." \
        --output-format json 2>/dev/null) || AGENT_JSON='{}'
    ELAPSED=$(( $(date +%s) - START ))

    ANSWER=$(echo "$AGENT_JSON" | jq -r '.result // "AGENT_ERROR"')
    COST=$(echo "$AGENT_JSON" | jq -r '.total_cost_usd // 0')
    IN_TOK=$(echo "$AGENT_JSON" | jq -r '(.usage.input_tokens // 0) + (.usage.cache_creation_input_tokens // 0) + (.usage.cache_read_input_tokens // 0)')
    OUT_TOK=$(echo "$AGENT_JSON" | jq -r '.usage.output_tokens // 0')
    TURNS=$(echo "$AGENT_JSON" | jq -r '.num_turns // 0')

    JUDGE_PROMPT="You are judging a question-answering benchmark. Score the candidate answer against the gold answer with semantic-match criteria: paraphrase-tolerant and format-tolerant, but all substantive facts in the gold answer must be present and correct in the candidate. Reply with exactly one word: CORRECT or INCORRECT.

Question: $PROMPT
Gold answer: $GOLD
Candidate answer: $ANSWER"
    VERDICT=$(claude -p "$JUDGE_PROMPT" --model "$JUDGE_MODEL" --max-turns 1 --output-format json 2>/dev/null | jq -r '.result // "JUDGE_ERROR"' | tr -d '[:space:]')

    jq -cn \
        --arg corpus "$CORPUS" --arg condition "$CONDITION" --arg qid "$QID" \
        --arg family "$FAMILY" --arg answer "$ANSWER" --arg verdict "$VERDICT" \
        --argjson cost "$COST" --argjson in_tok "$IN_TOK" --argjson out_tok "$OUT_TOK" \
        --argjson turns "$TURNS" --argjson elapsed "$ELAPSED" \
        '{corpus: $corpus, condition: $condition, qid: $qid, family: $family,
          verdict: $verdict, cost_usd: $cost, input_tokens: $in_tok,
          output_tokens: $out_tok, turns: $turns, elapsed_s: $elapsed,
          answer: $answer}' >> "$OUT"
    echo "   verdict=$VERDICT tokens=$((IN_TOK + OUT_TOK)) turns=$TURNS cost=\$$COST time=${ELAPSED}s"
done

echo
echo "== Summary: $OUT"
jq -s '
  {n: length,
   correct: map(select(.verdict == "CORRECT")) | length,
   total_tokens: map(.input_tokens + .output_tokens) | add,
   total_cost_usd: map(.cost_usd) | add,
   tokens_per_correct: (if (map(select(.verdict == "CORRECT")) | length) > 0
     then ((map(.input_tokens + .output_tokens) | add) / (map(select(.verdict == "CORRECT")) | length) | floor)
     else null end)}' "$OUT"
