# Retrieval eval

HearthFS ships a two-tier harness for measuring how well its search finds
the right files in a corpus of your choosing:

1. **Retrieval tier** (free, deterministic). The
   `crates/hearthfs-cli/examples/retrieval_eval.rs` example indexes each
   corpus with `hearthfs-core`, runs every question's prompt as a search
   (the same chunked BM25 + embedding fusion the `@search` magic path
   uses), and scores where the gold files rank.
2. **Agent tier** (paid). `scripts/eval/` runs a headless Claude Code agent
   (`claude -p`) on every question twice, once over the raw corpus folder
   and once over a HearthFS mount of it, and has an LLM judge each answer
   against the gold answer. The metric is tokens spent per correct answer.

Re-run the retrieval tier before changing anything that affects ranking:
the fusion weight (`Embedder::fusion_weight`), the path-column weight in
`fts.rs`, chunk sizes, or the FTS OR fallback. Run it on corpora of the
sizes you care about, because small corpora hide regressions that only show
up at a thousand files or more.

## Corpus format

```
<root>/
  <corpus>/
    question.json
    data/
      … the files to search, in any folder layout …
```

`question.json` is a JSON array with one object per question:

| field | meaning |
|---|---|
| `id` | question id, unique within the corpus |
| `family` | free-form category used to break results down (e.g. `fact-lookup`, `multi-file`) |
| `prompt` | the question; used verbatim as the search query and as the agent's prompt |
| `gold_file_ids` | the files that hold the answer, as paths relative to the corpus folder (`data/…`) |
| `gold_answer` | the reference answer the judge compares against (agent tier only) |

```json
[
  {
    "id": "q01",
    "family": "fact-lookup",
    "prompt": "How much money did the board approve for the rain barrel system?",
    "gold_file_ids": ["data/meetings/2026-03-12-board.md"],
    "gold_answer": "$640"
  }
]
```

Point the harness at a corpus folder, or at a root folder, in which case
every immediate subfolder that holds a `question.json` is a corpus.

## The bundled sample

`eval/fixtures/sample/` is a tiny invented corpus: 11 files of a fictional
community garden (board minutes, an invoice email, a calendar invite, CSV
rotas and ledgers, notes) and 5 questions. It is small enough that every
question should hit at rank 1, so it checks the harness and shows the
format; it does not measure retrieval quality. Meaningful numbers need
corpora large enough that an agent cannot simply read every file: hundreds
to thousands of files.

## Running the retrieval tier

```bash
cargo run --release --example retrieval_eval -- eval/fixtures
cargo run --release --example retrieval_eval -- <root> --corpora a,b   # only these corpora
cargo run --release --example retrieval_eval -- <root> --no-embeddings # BM25 only
```

- **Embeddings** come from `HEARTHFS_EMBEDDER` (`local:<dir>` or
  `voyage:<model>`) or else from the default local model in `./models/` or
  `~/.hearthfs/models/` (see `scripts/fetch-model.sh`). Without a model the
  eval runs BM25 only.
- `HEARTHFS_EVAL_VEC_WEIGHT=<w>` overrides the vector leg's fusion weight.
  The default is the backend's production weight (`Embedder::fusion_weight`:
  0.05 for local models, 1.0 for Voyage), so sweep this before changing the
  production value.
- `HEARTHFS_EVAL_CACHE=<dir>` moves the index cache (default
  `$TMPDIR/hearthfs-eval/<corpus>/`, with one vector database per embedding
  backend). Re-runs skip unchanged files.
- Questions whose gold files are missing from the top 40 are printed to
  stderr as `miss: <corpus>/<id> (<family>)`.

Output is a Markdown table per corpus, per question family and overall:

- **hit@k**: the share of questions whose first gold file ranks in the top k.
- **all-gold@20**: the share of questions with *every* gold file in the top
  20 (what multi-file questions need).
- **MRR**: the mean of 1/rank of the first gold file (0 when it is not in
  the top 40).

## Running the agent tier

Requirements: macOS (the hearthfs condition mounts over NFS), the `claude`
CLI signed in, `jq`, and a release build (`cargo build --release`). Every
question costs one agent session plus one judge call, so check your usage
before running large corpora.

```bash
./scripts/eval/run_stage.sh <root> <corpus> [<corpus>...]           # raw, then hearthfs, per corpus
./scripts/eval/agent_eval.sh <root> <corpus> raw|hearthfs [question-id]  # one condition
./scripts/eval/summarize.sh [corpus-filter]                          # compare runs
```

- `run_stage.sh` mounts each corpus at `HEARTHFS_EVAL_MOUNT` (default
  `/tmp/hearthfs-eval-mount`) on port `HEARTHFS_EVAL_PORT` (default 11177)
  with `HEARTHFS_BIN` (default `target/release/hearthfs`), waits for the
  server's "initial indexing complete" log line before any agent starts,
  and unmounts afterwards.
- In the hearthfs condition the agent's system prompt describes the magic
  paths (`@search`, `@outline`, `@section`, `@tokens`); in the raw condition
  it is told to use grep, find, ls and file reads.
- `HEARTHFS_EVAL_MODEL` and `HEARTHFS_EVAL_JUDGE_MODEL` pick the agent and
  judge models (default `claude-sonnet-4-6`); `HEARTHFS_EVAL_MAX_TURNS`
  caps the agent's turns (default 25).
- Results append one JSON line per question to
  `$HEARTHFS_EVAL_RESULTS/<corpus>-<condition>.jsonl` (default
  `scripts/eval/results/`, which git ignores): verdict, tokens, cost, turns,
  time and the answer. Questions already recorded are skipped, so an
  interrupted run resumes. To repeat a run, first move the result files into
  an `archive-<name>/` subfolder; `summarize.sh` reports every run
  separately, so run-to-run variance stays visible.
- `seed_mount_vectors.sh <root> <corpus>` copies Voyage vectors that the
  retrieval tier already computed into a mount's index, so a Voyage-backed
  agent run does not pay to embed the corpus twice.

Caveats: with around ten questions per corpus, single runs are noisy, so
repeat a run before quoting a difference. The judge and the agent share a
model family. Token totals include the agent harness's fixed per-turn
overhead in both conditions.
