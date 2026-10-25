use std::path::Path;

use anyhow::{Context, Result};
use tract_onnx::prelude::*;

type Model = SimplePlan<TypedFact, Box<dyn TypedOp>, Graph<TypedFact, Box<dyn TypedOp>>>;

/// Default wordpiece context for local models. MiniLM was trained at 256;
/// BERT-family models accept up to 512 (set HEARTHFS_EMBED_MAX_LEN).
const DEFAULT_LOCAL_MAX_LEN: usize = 256;

/// Texts per Voyage API request (well under the 128-input cap).
const VOYAGE_BATCH: usize = 64;

/// Embedding generator with pluggable backends.
///
/// - `LocalOnnx`: BERT-family ONNX model via tract (pure Rust, on-device).
///   `embed` is `&self` and safe to call concurrently: tract's
///   `SimplePlan::run` shares weights and builds per-call state, and
///   `Tokenizer::encode` is also `&self`.
/// - `Voyage`: Voyage AI embeddings API (opt-in cloud backend; sync HTTP via
///   ureq so it is safe to call from any thread, including async contexts).
///
/// Select with `HEARTHFS_EMBEDDER`:
///   `local:<model-dir>` | `voyage:<model>` (key from $VOYAGE_API_KEY or
///   ~/.config/hearthfs/voyage.key). Unset → caller's default local model.
pub struct Embedder {
    backend: Backend,
    dim: usize,
    id: String,
}

enum Backend {
    LocalOnnx {
        model: Model,
        tokenizer: tokenizers::Tokenizer,
        max_len: usize,
    },
    Voyage {
        model: String,
        api_key: String,
    },
}

impl Embedder {
    /// Load a local ONNX model from a directory containing exactly one
    /// `*.onnx` file plus `tokenizer.json`.
    pub fn load(model_dir: &Path) -> Result<Self> {
        let onnx_path = std::fs::read_dir(model_dir)
            .with_context(|| format!("cannot read model dir {}", model_dir.display()))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().map(|e| e == "onnx").unwrap_or(false))
            .with_context(|| format!("no .onnx model in {}", model_dir.display()))?;
        let tokenizer_path = model_dir.join("tokenizer.json");
        anyhow::ensure!(
            tokenizer_path.exists(),
            "tokenizer not found at {}",
            tokenizer_path.display()
        );

        let max_len: usize = std::env::var("HEARTHFS_EMBED_MAX_LEN")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_LOCAL_MAX_LEN);

        let model = tract_onnx::onnx()
            .model_for_path(&onnx_path)
            .context("failed to load ONNX model")?
            .with_input_fact(0, InferenceFact::dt_shape(i64::datum_type(), &[1, max_len as i64]))?
            .with_input_fact(1, InferenceFact::dt_shape(i64::datum_type(), &[1, max_len as i64]))?
            .with_input_fact(2, InferenceFact::dt_shape(i64::datum_type(), &[1, max_len as i64]))?
            .into_optimized()
            .context("failed to optimize model")?
            .into_runnable()
            .context("failed to make model runnable")?;

        let tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("failed to load tokenizer: {e}"))?;

        let id = onnx_path
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_else(|| "local".into());

        let mut embedder = Self {
            backend: Backend::LocalOnnx {
                model,
                tokenizer,
                max_len,
            },
            dim: 0,
            id: format!("{id}@{max_len}"),
        };
        embedder.dim = embedder.embed("dimension probe")?.len();
        Ok(embedder)
    }

    /// Create a Voyage AI API backend. Probes the API once to learn the
    /// embedding dimension.
    pub fn voyage(model: &str, api_key: &str) -> Result<Self> {
        let mut embedder = Self {
            backend: Backend::Voyage {
                model: model.to_string(),
                api_key: api_key.to_string(),
            },
            dim: 0,
            id: model.to_string(),
        };
        embedder.dim = embedder.embed("dimension probe")?.len();
        Ok(embedder)
    }

    /// Build from HEARTHFS_EMBEDDER if set; Ok(None) means "not configured,
    /// use the caller's default".
    pub fn from_env() -> Result<Option<Self>> {
        let Ok(spec) = std::env::var("HEARTHFS_EMBEDDER") else {
            return Ok(None);
        };
        match spec.split_once(':') {
            Some(("local", dir)) => Ok(Some(Self::load(Path::new(dir))?)),
            Some(("voyage", model)) => {
                let key = std::env::var("VOYAGE_API_KEY").ok().or_else(|| {
                    let path = std::env::var_os("HOME").map(|h| {
                        Path::new(&h).join(".config").join("hearthfs").join("voyage.key")
                    })?;
                    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
                });
                let key = key.context(
                    "voyage backend needs VOYAGE_API_KEY or ~/.config/hearthfs/voyage.key",
                )?;
                Ok(Some(Self::voyage(model, &key)?))
            }
            _ => anyhow::bail!("invalid HEARTHFS_EMBEDDER '{spec}' (local:<dir> | voyage:<model>)"),
        }
    }

    /// Stable identifier for cache separation (model + probe geometry).
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Embedding dimension.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Fusion weight this backend's vectors earn against BM25 (1.0). Small
    /// local models are a recall backfill (0.05); strong API embeddings add
    /// top-20 recall at scale and earn a real weight. Both were measured with
    /// the retrieval eval (docs/retrieval-eval.md): re-run it before changing
    /// them. HEARTHFS_VEC_WEIGHT overrides.
    pub fn fusion_weight(&self) -> f64 {
        if let Some(w) = std::env::var("HEARTHFS_VEC_WEIGHT")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            return w;
        }
        match &self.backend {
            Backend::LocalOnnx { .. } => 0.05,
            Backend::Voyage { .. } => 1.0,
        }
    }

    /// How many characters of a chunk are worth sending as one probe.
    pub fn probe_chars(&self) -> usize {
        match &self.backend {
            // ~3.5 chars per wordpiece token for English prose.
            Backend::LocalOnnx { max_len, .. } => max_len * 7 / 2,
            // Voyage handles 32k tokens; send whole chunks.
            Backend::Voyage { .. } => usize::MAX,
        }
    }

    /// Embed a document/passage.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        match &self.backend {
            Backend::LocalOnnx { .. } => self.embed_local(text),
            Backend::Voyage { .. } => {
                let mut out = self.embed_voyage(&[text.to_string()], "document")?;
                out.pop().context("voyage returned no embedding")
            }
        }
    }

    /// Embed a search query (some backends embed queries differently).
    pub fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        match &self.backend {
            Backend::LocalOnnx { .. } => self.embed_local(text),
            Backend::Voyage { .. } => {
                let mut out = self.embed_voyage(&[text.to_string()], "query")?;
                out.pop().context("voyage returned no embedding")
            }
        }
    }

    /// Embed many document probes. Local models fan out across threads;
    /// API backends batch per request. Result order matches input order.
    pub fn embed_batch(&self, texts: &[String]) -> Vec<Result<Vec<f32>>> {
        if texts.is_empty() {
            return vec![];
        }
        match &self.backend {
            Backend::LocalOnnx { .. } => {
                let workers = std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4)
                    .min(texts.len())
                    .min(8);
                let per_worker = texts.len().div_ceil(workers);
                std::thread::scope(|scope| {
                    let handles: Vec<_> = texts
                        .chunks(per_worker)
                        .map(|slice| {
                            scope.spawn(move || {
                                slice.iter().map(|t| self.embed_local(t)).collect::<Vec<_>>()
                            })
                        })
                        .collect();
                    handles
                        .into_iter()
                        .flat_map(|h| h.join().expect("embed worker panicked"))
                        .collect()
                })
            }
            Backend::Voyage { .. } => {
                let mut results = Vec::with_capacity(texts.len());
                for batch in texts.chunks(VOYAGE_BATCH) {
                    match self.embed_voyage(batch, "document") {
                        Ok(vecs) => results.extend(vecs.into_iter().map(Ok)),
                        Err(e) => {
                            for _ in batch {
                                results.push(Err(anyhow::anyhow!("voyage batch failed: {e}")));
                            }
                        }
                    }
                }
                results
            }
        }
    }

    fn embed_local(&self, text: &str) -> Result<Vec<f32>> {
        let Backend::LocalOnnx {
            model,
            tokenizer,
            max_len,
        } = &self.backend
        else {
            unreachable!("embed_local on non-local backend")
        };
        let max_len = *max_len;

        let encoding = tokenizer
            .encode(text, true)
            .map_err(|e| anyhow::anyhow!("tokenization failed: {e}"))?;

        let mut input_ids: Vec<i64> = encoding.get_ids().iter().map(|&id| id as i64).collect();
        let mut attention_mask: Vec<i64> =
            encoding.get_attention_mask().iter().map(|&m| m as i64).collect();
        let mut token_type_ids: Vec<i64> =
            encoding.get_type_ids().iter().map(|&t| t as i64).collect();

        // Truncate or pad to max_len
        input_ids.truncate(max_len);
        attention_mask.truncate(max_len);
        token_type_ids.truncate(max_len);
        while input_ids.len() < max_len {
            input_ids.push(0);
            attention_mask.push(0);
            token_type_ids.push(0);
        }
        let mask = attention_mask.clone();

        let input_ids =
            tract_ndarray::Array2::from_shape_vec((1, max_len), input_ids)?.into_tensor();
        let attention_mask =
            tract_ndarray::Array2::from_shape_vec((1, max_len), attention_mask)?.into_tensor();
        let token_type_ids =
            tract_ndarray::Array2::from_shape_vec((1, max_len), token_type_ids)?.into_tensor();

        // SimplePlan::run is &self; weights are shared and per-call state is
        // created internally, so no lock is needed.
        let outputs = model.run(tvec![
            input_ids.into(),
            attention_mask.into(),
            token_type_ids.into(),
        ])?;

        // Output is [1, max_len, hidden] token embeddings. Mean-pool over
        // the *attended* positions only — pooling padding dilutes short
        // texts badly.
        let output = outputs[0]
            .to_array_view::<f32>()
            .context("failed to extract output tensor")?;

        let shape = output.shape();
        let seq_len = shape[1];
        let hidden = shape[2];

        let mut pooled = vec![0.0f32; hidden];
        let mut count = 0.0f32;
        for i in 0..seq_len {
            if mask[i] == 0 {
                continue;
            }
            count += 1.0;
            for j in 0..hidden {
                pooled[j] += output[[0, i, j]];
            }
        }
        if count > 0.0 {
            for v in &mut pooled {
                *v /= count;
            }
        }

        // L2 normalize
        let norm: f32 = pooled.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in &mut pooled {
                *v /= norm;
            }
        }

        Ok(pooled)
    }

    fn embed_voyage(&self, texts: &[String], input_type: &str) -> Result<Vec<Vec<f32>>> {
        let Backend::Voyage { model, api_key } = &self.backend else {
            unreachable!("embed_voyage on non-voyage backend")
        };

        let body = serde_json::json!({
            "input": texts,
            "model": model,
            "input_type": input_type,
        });

        // Retry on rate limits / transient errors with simple backoff.
        let mut last_err = None;
        for attempt in 0..5 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(500 * (1 << attempt)));
            }
            let response = ureq::post("https://api.voyageai.com/v1/embeddings")
                .set("Authorization", &format!("Bearer {api_key}"))
                .set("Content-Type", "application/json")
                .send_json(body.clone());
            match response {
                Ok(resp) => {
                    let json: serde_json::Value =
                        resp.into_json().context("voyage: invalid JSON response")?;
                    let data = json["data"]
                        .as_array()
                        .context("voyage: missing data array")?;
                    let mut out = vec![Vec::new(); texts.len()];
                    for item in data {
                        let idx = item["index"].as_u64().context("voyage: missing index")?
                            as usize;
                        let vec: Vec<f32> = item["embedding"]
                            .as_array()
                            .context("voyage: missing embedding")?
                            .iter()
                            .filter_map(|v| v.as_f64().map(|f| f as f32))
                            .collect();
                        anyhow::ensure!(idx < out.len(), "voyage: index out of range");
                        out[idx] = vec;
                    }
                    return Ok(out);
                }
                Err(ureq::Error::Status(code, resp)) if code == 429 || code >= 500 => {
                    last_err = Some(anyhow::anyhow!(
                        "voyage HTTP {code}: {}",
                        resp.into_string().unwrap_or_default()
                    ));
                }
                Err(e) => return Err(anyhow::anyhow!("voyage request failed: {e}")),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("voyage: retries exhausted")))
    }
}

/// Cosine similarity between two normalized vectors.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 0.001);

        let c = vec![0.0, 1.0, 0.0];
        assert!((cosine_similarity(&a, &c)).abs() < 0.001);
    }

    #[test]
    fn test_embedder_loads_if_model_present() {
        let model_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("models");

        if !model_dir.join("all-MiniLM-L6-v2.onnx").exists() {
            eprintln!(
                "skipping embedder test: model not found at {}",
                model_dir.display()
            );
            return;
        }

        let embedder = Embedder::load(&model_dir).unwrap();
        assert_eq!(embedder.dim(), 384);
        assert!(embedder.id().starts_with("all-minilm-l6-v2"));

        let v1 = embedder.embed("hello world").unwrap();
        let v2 = embedder.embed("hello world").unwrap();
        assert!((cosine_similarity(&v1, &v2) - 1.0).abs() < 0.001);

        let batch = embedder.embed_batch(&["alpha".into(), "beta".into()]);
        assert_eq!(batch.len(), 2);
        assert!(batch.iter().all(|r| r.is_ok()));
    }
}
