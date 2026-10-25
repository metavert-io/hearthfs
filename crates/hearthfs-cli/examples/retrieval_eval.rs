//! Retrieval evaluation: does HearthFS's search index surface the right files?
//!
//! For every question in a corpus, runs the question's prompt as a search
//! (the same chunked BM25 + optional embeddings fusion the `@search` magic
//! path uses) and scores where the gold files rank: hit@k, all-gold@20 and
//! MRR, per corpus, per question family, and overall. No LLM is involved,
//! so it is free and deterministic. The corpus format is documented in
//! docs/retrieval-eval.md.
//!
//! Usage:
//!   cargo run --release --example retrieval_eval -- <root> [--corpora a,b] [--no-embeddings]
//!
//! `<root>` is either one corpus (a folder holding `question.json` and
//! `data/`) or a folder of corpora (every subfolder holding a
//! `question.json`). Try it on the bundled sample:
//!   cargo run --release --example retrieval_eval -- eval/fixtures
//!
//! Environment:
//!   HEARTHFS_EMBEDDER          embedding backend (local:<dir> | voyage:<model>);
//!                              default: the local model in ./models or
//!                              ~/.hearthfs/models, if present
//!   HEARTHFS_EVAL_VEC_WEIGHT   fusion weight of the vector leg (default: the
//!                              backend's production weight, see
//!                              `Embedder::fusion_weight`)
//!   HEARTHFS_EVAL_CACHE        index cache root (default $TMPDIR/hearthfs-eval);
//!                              re-runs skip unchanged files

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use serde::Deserialize;

use hearthfs_core::embeddings::Embedder;
use hearthfs_core::fts::FtsIndex;
use hearthfs_core::indexer::Indexer;
use hearthfs_core::vector_store::VectorStore;
use hearthfs_source_fs::FsSource;

const USAGE: &str = "usage: retrieval_eval <root> [--corpora a,b] [--no-embeddings]";

#[derive(Deserialize)]
struct Question {
    id: String,
    family: String,
    prompt: String,
    gold_file_ids: Vec<String>,
    #[allow(dead_code)]
    gold_answer: String,
}

#[derive(Default)]
struct Tally {
    questions: usize,
    hit_at_1: usize,
    hit_at_5: usize,
    hit_at_10: usize,
    hit_at_20: usize,
    hit_at_40: usize,
    all_gold_at_20: usize,
    mrr_sum: f64,
}

impl Tally {
    fn add(&mut self, first_gold_rank: Option<usize>, all_gold_in_20: bool) {
        self.questions += 1;
        if let Some(rank) = first_gold_rank {
            if rank <= 1 {
                self.hit_at_1 += 1;
            }
            if rank <= 5 {
                self.hit_at_5 += 1;
            }
            if rank <= 10 {
                self.hit_at_10 += 1;
            }
            if rank <= 20 {
                self.hit_at_20 += 1;
            }
            if rank <= 40 {
                self.hit_at_40 += 1;
            }
            self.mrr_sum += 1.0 / rank as f64;
        }
        if all_gold_in_20 {
            self.all_gold_at_20 += 1;
        }
    }

    fn row(&self, label: &str) -> String {
        let n = self.questions.max(1) as f64;
        format!(
            "| {label} | {} | {:.0}% | {:.0}% | {:.0}% | {:.0}% | {:.0}% | {:.0}% | {:.3} |",
            self.questions,
            100.0 * self.hit_at_1 as f64 / n,
            100.0 * self.hit_at_5 as f64 / n,
            100.0 * self.hit_at_10 as f64 / n,
            100.0 * self.hit_at_20 as f64 / n,
            100.0 * self.hit_at_40 as f64 / n,
            100.0 * self.all_gold_at_20 as f64 / n,
            self.mrr_sum / n,
        )
    }
}

/// Weighted reciprocal-rank fusion of the FTS and vector result lists —
/// the same shape as the `@search` fusion in passthrough.rs.
fn fuse(fts: &[String], vec: &[String], vec_weight: f64, limit: usize) -> Vec<String> {
    const K: f64 = 60.0;
    let mut scores: Vec<(String, f64)> = Vec::new();
    let mut bump = |path: &str, rank: usize, weight: f64| {
        let score = weight / (K + rank as f64 + 1.0);
        if let Some(entry) = scores.iter_mut().find(|(p, _)| p == path) {
            entry.1 += score;
        } else {
            scores.push((path.to_string(), score));
        }
    };
    for (rank, path) in fts.iter().enumerate() {
        bump(path, rank, 1.0);
    }
    for (rank, path) in vec.iter().enumerate() {
        bump(path, rank, vec_weight);
    }
    scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    scores.into_iter().take(limit).map(|(p, _)| p).collect()
}

/// The corpora under `root`: `root` itself when it holds a question.json,
/// otherwise every immediate subfolder that does, sorted by name.
fn discover_corpora(root: &Path) -> Result<Vec<PathBuf>> {
    if root.join("question.json").is_file() {
        return Ok(vec![root.to_path_buf()]);
    }
    let mut corpora: Vec<PathBuf> = std::fs::read_dir(root)
        .with_context(|| format!("cannot read {}", root.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.join("question.json").is_file())
        .collect();
    corpora.sort();
    Ok(corpora)
}

fn corpus_name(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| dir.display().to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = PathBuf::from(args.first().filter(|a| !a.starts_with("--")).context(USAGE)?);
    let use_embeddings = !args.iter().any(|a| a == "--no-embeddings");
    let corpus_filter: Option<Vec<String>> = args
        .iter()
        .position(|a| a == "--corpora")
        .and_then(|i| args.get(i + 1))
        .map(|list| list.split(',').map(str::to_string).collect());

    let embedder = if use_embeddings {
        // HEARTHFS_EMBEDDER (local:<dir> | voyage:<model>) wins; otherwise
        // fall back to the default local model directory.
        let from_env = Embedder::from_env().unwrap_or_else(|e| {
            eprintln!("embeddings: HEARTHFS_EMBEDDER failed ({e}); trying default");
            None
        });
        let embedder = from_env.or_else(|| {
            ["models", "../models"]
                .iter()
                .map(Path::new)
                .find(|p| p.join("all-MiniLM-L6-v2.onnx").exists())
                .map(Path::to_path_buf)
                .or_else(|| dirs_model().filter(|p| p.join("all-MiniLM-L6-v2.onnx").exists()))
                .and_then(|dir| Embedder::load(&dir).ok())
        });
        match embedder {
            Some(e) => {
                eprintln!("embeddings: backend {} (dim {})", e.id(), e.dim());
                Some(Arc::new(e))
            }
            None => {
                eprintln!("embeddings: no backend available; FTS only");
                None
            }
        }
    } else {
        None
    };
    // Same default as production; the env var sweeps fusion weights
    // without rebuilding.
    let vec_weight: f64 = std::env::var("HEARTHFS_EVAL_VEC_WEIGHT")
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(|| embedder.as_ref().map(|e| e.fusion_weight()))
        .unwrap_or(0.0);
    if embedder.is_some() {
        eprintln!("fusion: vector weight {vec_weight}");
    }

    let mut corpora = discover_corpora(&root)?;
    if let Some(filter) = &corpus_filter {
        corpora.retain(|dir| filter.contains(&corpus_name(dir)));
    }
    anyhow::ensure!(
        !corpora.is_empty(),
        "no corpora found under {} (a corpus is a folder with question.json and data/)",
        root.display()
    );

    let cache_root = std::env::var_os("HEARTHFS_EVAL_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("hearthfs-eval"));
    let mut overall = Tally::default();
    let mut family_tallies: Vec<(String, Tally)> = Vec::new();

    println!("# Retrieval eval — HearthFS\n");
    println!("| corpus | files | n | hit@1 | hit@5 | hit@10 | hit@20 | hit@40 | all-gold@20 | MRR |");
    println!("|---|---|---|---|---|---|---|---|---|---|");

    for corpus in &corpora {
        let name = corpus_name(corpus);
        let data_dir = corpus.join("data");
        let questions: Vec<Question> =
            serde_json::from_slice(&std::fs::read(corpus.join("question.json"))?)
                .with_context(|| format!("{name}/question.json"))?;

        let index_dir = cache_root.join(&name);
        std::fs::create_dir_all(&index_dir)?;
        let fts = Arc::new(FtsIndex::open(&index_dir.join("fts.db"))?);
        let source = Arc::new(
            FsSource::new(data_dir.clone()).with_context(|| format!("{name}: no data/ folder"))?,
        );

        let vectors = match &embedder {
            Some(e) => {
                // Separate cache per backend so model comparisons coexist.
                let sanitized: String = e
                    .id()
                    .chars()
                    .map(|c| if c.is_alphanumeric() { c } else { '-' })
                    .collect();
                let store = VectorStore::open(
                    &index_dir.join(format!("vectors-{sanitized}.db")),
                    e.dim(),
                )?;
                store.set_model(e.id())?;
                Some(Arc::new(store))
            }
            None => None,
        };

        let mut indexer = Indexer::new(source, fts.clone());
        if let (Some(e), Some(v)) = (&embedder, &vectors) {
            indexer = indexer.with_embeddings(e.clone(), v.clone());
        }
        let started = Instant::now();
        let indexed = indexer.full_scan().await?;
        let file_count = fts.file_count()?;
        eprintln!(
            "{name}: indexed {indexed} new files ({file_count} total) in {:.1}s",
            started.elapsed().as_secs_f64()
        );

        let mut corpus_tally = Tally::default();
        for q in &questions {
            let fts_paths: Vec<String> = fts
                .search(&q.prompt, 20)
                .unwrap_or_default()
                .into_iter()
                .map(|r| r.path)
                .collect();
            let vec_paths: Vec<String> = match (&embedder, &vectors) {
                (Some(e), Some(v)) => e
                    .embed_query(&q.prompt)
                    .ok()
                    .and_then(|qv| v.search(&qv, 40).ok())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|r| r.path)
                    .collect(),
                _ => vec![],
            };
            let ranked = fuse(&fts_paths, &vec_paths, vec_weight, 40);

            // gold_file_ids are relative to the corpus folder ("data/...");
            // index paths are relative to data/.
            let gold: Vec<&str> = q
                .gold_file_ids
                .iter()
                .map(|g| g.strip_prefix("data/").unwrap_or(g))
                .collect();

            let first_gold_rank = ranked
                .iter()
                .position(|p| gold.contains(&p.as_str()))
                .map(|i| i + 1);
            let all_gold = gold.iter().all(|g| ranked.iter().take(20).any(|p| p == g));

            corpus_tally.add(first_gold_rank, all_gold);
            overall.add(first_gold_rank, all_gold);
            match family_tallies.iter_mut().find(|(f, _)| *f == q.family) {
                Some((_, t)) => t.add(first_gold_rank, all_gold),
                None => {
                    let mut t = Tally::default();
                    t.add(first_gold_rank, all_gold);
                    family_tallies.push((q.family.clone(), t));
                }
            }

            if first_gold_rank.is_none() {
                eprintln!("  miss: {name}/{} ({})", q.id, q.family);
            }
        }
        println!("{}", corpus_tally.row(&format!("{name} | {file_count}")));
    }

    println!("\n## By question family\n");
    println!("| family | n | hit@1 | hit@5 | hit@10 | hit@20 | hit@40 | all-gold@20 | MRR |");
    println!("|---|---|---|---|---|---|---|---|---|");
    for (name, tally) in &family_tallies {
        println!("{}", tally.row(name));
    }
    println!("\n## Overall\n");
    println!("| | n | hit@1 | hit@5 | hit@10 | hit@20 | hit@40 | all-gold@20 | MRR |");
    println!("|---|---|---|---|---|---|---|---|---|");
    println!("{}", overall.row("all"));

    Ok(())
}

fn dirs_model() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".hearthfs").join("models"))
}
