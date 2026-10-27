use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use rusqlite::Connection;

use crate::embeddings::cosine_similarity;

/// Vector store for embeddings, backed by SQLite.
///
/// Stores float32 vectors as blobs. For v0 scale (<100K files),
/// brute-force cosine similarity is fast enough.
pub struct VectorStore {
    conn: Mutex<Connection>,
    dim: usize,
}

/// A vector search result.
#[derive(Debug, Clone)]
pub struct VectorResult {
    pub path: String,
    pub similarity: f32,
}

impl VectorStore {
    pub fn open(path: &Path, dim: usize) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            ",
        )?;

        // v2: one embedding per file replaced by per-chunk embeddings. Old
        // stores are caches — drop and force a re-embed rather than migrate.
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < 2 {
            conn.execute_batch(
                "
                DROP TABLE IF EXISTS vectors;
                PRAGMA user_version = 2;
                ",
            )?;
        }
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS vectors (
                path TEXT NOT NULL,
                chunk_idx INTEGER NOT NULL,
                embedding BLOB NOT NULL,
                content_hash TEXT NOT NULL,
                PRIMARY KEY (path, chunk_idx)
            );
            ",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
            dim,
        })
    }

    /// Record which embedding model produced this store's vectors. If the
    /// model (or probe geometry) changed, existing vectors are incompatible
    /// — wipe them so the indexer re-embeds.
    pub fn set_model(&self, model_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )?;
        let current: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'model_id'", [], |r| {
                r.get(0)
            })
            .ok();
        if current.as_deref() != Some(model_id) {
            // Different model, or vectors of unknown provenance (store
            // predates model tracking): either way they can't be trusted.
            conn.execute("DELETE FROM vectors", [])?;
            conn.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('model_id', ?1)",
                [model_id],
            )?;
        }
        Ok(())
    }

    /// Store one chunk's embedding for a file.
    pub fn upsert_chunk(
        &self,
        path: &str,
        chunk_idx: usize,
        embedding: &[f32],
        content_hash: &str,
    ) -> Result<()> {
        anyhow::ensure!(
            embedding.len() == self.dim,
            "embedding dimension {} does not match store dimension {}",
            embedding.len(),
            self.dim
        );
        let blob = floats_to_bytes(embedding);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO vectors (path, chunk_idx, embedding, content_hash) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![path, chunk_idx as i64, blob, content_hash],
        )?;
        Ok(())
    }

    /// Remove all of a file's chunk embeddings.
    pub fn remove_file(&self, path: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM vectors WHERE path = ?1", [path])?;
        Ok(())
    }

    /// Check if a file's embedding is current.
    pub fn is_current(&self, path: &str, content_hash: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM vectors WHERE path = ?1 AND content_hash = ?2",
            [path, content_hash],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Find the most similar files to a query embedding. Chunk similarities
    /// aggregate to file level: each file is scored by its best chunk.
    pub fn search(&self, query: &[f32], limit: usize) -> Result<Vec<VectorResult>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT path, embedding FROM vectors")?;
        let rows: Vec<(String, Vec<u8>)> = stmt
            .query_map([], |row| {
                let path: String = row.get(0)?;
                let blob: Vec<u8> = row.get(1)?;
                Ok((path, blob))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        drop(conn);

        let mut best: HashMap<String, f32> = HashMap::new();
        for (path, blob) in &rows {
            let embedding = bytes_to_floats(blob);
            let similarity = cosine_similarity(query, &embedding);
            let entry = best.entry(path.clone()).or_insert(f32::MIN);
            if similarity > *entry {
                *entry = similarity;
            }
        }

        let mut results: Vec<VectorResult> = best
            .into_iter()
            .map(|(path, similarity)| VectorResult { path, similarity })
            .collect();
        results.sort_by(|a, b| b.similarity.partial_cmp(&a.similarity).unwrap());
        results.truncate(limit);
        Ok(results)
    }

    /// Number of stored vectors.
    pub fn count(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM vectors", [], |row| row.get(0))?;
        Ok(count)
    }
}

fn floats_to_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn bytes_to_floats(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_store_and_search() {
        let store = VectorStore::open(Path::new(":memory:"), 3).unwrap();
        store
            .upsert_chunk("a.txt", 0, &[1.0, 0.0, 0.0], "h1")
            .unwrap();
        store
            .upsert_chunk("b.txt", 0, &[0.0, 1.0, 0.0], "h2")
            .unwrap();
        store
            .upsert_chunk("c.txt", 0, &[0.9, 0.1, 0.0], "h3")
            .unwrap();

        let results = store.search(&[1.0, 0.0, 0.0], 2).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].path, "a.txt");
        assert!(results[0].similarity > results[1].similarity);
    }

    #[test]
    fn test_set_model_wipes_on_change_and_unknown_provenance() {
        let store = VectorStore::open(Path::new(":memory:"), 3).unwrap();
        store
            .upsert_chunk("a.txt", 0, &[1.0, 0.0, 0.0], "h1")
            .unwrap();
        // Store predates model tracking: adopting a model must wipe.
        store.set_model("model-a").unwrap();
        assert_eq!(store.count().unwrap(), 0);

        store
            .upsert_chunk("a.txt", 0, &[1.0, 0.0, 0.0], "h1")
            .unwrap();
        store.set_model("model-a").unwrap();
        assert_eq!(store.count().unwrap(), 1, "same model keeps vectors");

        store.set_model("model-b").unwrap();
        assert_eq!(store.count().unwrap(), 0, "model change wipes");
    }

    #[test]
    fn test_file_scored_by_best_chunk() {
        let store = VectorStore::open(Path::new(":memory:"), 3).unwrap();
        // b.txt's best chunk beats a.txt's only chunk; each file appears once.
        store
            .upsert_chunk("a.txt", 0, &[0.8, 0.2, 0.0], "h1")
            .unwrap();
        store
            .upsert_chunk("b.txt", 0, &[0.0, 1.0, 0.0], "h2")
            .unwrap();
        store
            .upsert_chunk("b.txt", 1, &[1.0, 0.0, 0.0], "h2")
            .unwrap();

        let results = store.search(&[1.0, 0.0, 0.0], 10).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].path, "b.txt");
    }
}
