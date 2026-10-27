use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use rusqlite::Connection;

/// Target chunk size for FTS rows (~1500 tokens of text).
const FTS_CHUNK_BYTES: usize = 5250;

/// BM25 weight for the path column relative to content (1.0). Paths are
/// dense descriptive metadata; weighting them up raised large-corpus MRR
/// substantially (5k files: 0.19->0.31, 10k: 0.19->0.26) with flat impact
/// on small corpora. Re-run the retrieval eval (docs/retrieval-eval.md)
/// before changing it.
const DEFAULT_PATH_WEIGHT: f64 = 6.0;

/// Full-text search index backed by SQLite FTS5.
///
/// Content is stored as per-chunk rows (line-aligned, ~FTS_CHUNK_BYTES each)
/// so BM25 scores local passages instead of whole documents; searches
/// aggregate chunks back to file-level results.
///
/// Thread-safe via internal Mutex (rusqlite::Connection is not Sync).
pub struct FtsIndex {
    conn: Mutex<Connection>,
}

/// A search result with path, snippet, and BM25 rank.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub path: String,
    pub snippet: String,
    pub rank: f64,
}

impl FtsIndex {
    /// Open (or create) the FTS index at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;

            CREATE TABLE IF NOT EXISTS indexed_files (
                path TEXT PRIMARY KEY,
                content_hash TEXT NOT NULL,
                indexed_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now')),
                size INTEGER NOT NULL,
                token_estimate INTEGER NOT NULL
            );
            ",
        )?;

        // v2: whole-file rows replaced by per-chunk rows. Old indexes are
        // caches — drop and force a re-index rather than migrate in place.
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < 2 {
            conn.execute_batch(
                "
                DROP TABLE IF EXISTS fts_content;
                DELETE FROM indexed_files;
                PRAGMA user_version = 2;
                ",
            )?;
        }
        conn.execute_batch(
            "
            CREATE VIRTUAL TABLE IF NOT EXISTS fts_chunks USING fts5(
                path,
                content,
                tokenize = 'porter unicode61'
            );
            ",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Index a file's text content as chunks. Replaces any existing entry.
    pub fn index_file(&self, path: &str, content: &str, content_hash: &str) -> Result<()> {
        let token_estimate = crate::tokens::estimate_tokens(content.as_bytes()) as i64;
        let size = content.len() as i64;
        let conn = self.conn.lock().unwrap();

        conn.execute("DELETE FROM fts_chunks WHERE path = ?1", [path])?;
        conn.execute("DELETE FROM indexed_files WHERE path = ?1", [path])?;

        for chunk in crate::chunking::chunk_text(content, FTS_CHUNK_BYTES) {
            conn.execute(
                "INSERT INTO fts_chunks (path, content) VALUES (?1, ?2)",
                [path, chunk],
            )?;
        }
        conn.execute(
            "INSERT INTO indexed_files (path, content_hash, size, token_estimate) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![path, content_hash, size, token_estimate],
        )?;
        Ok(())
    }

    /// Remove a file from the index.
    pub fn remove_file(&self, path: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM fts_chunks WHERE path = ?1", [path])?;
        conn.execute("DELETE FROM indexed_files WHERE path = ?1", [path])?;
        Ok(())
    }

    /// Search for files matching a query. Returns results ranked by BM25.
    ///
    /// Terms are implicitly ANDed by FTS5, which returns nothing for long
    /// natural-language queries (every word must appear). If the AND query
    /// comes back thin, retry with the informative terms ORed so BM25 can
    /// rank partial matches.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let sanitized = sanitize_fts_query(query);
        let mut results = self.run_match(&sanitized, limit)?;

        if results.len() < limit {
            if let Some(or_query) = or_fallback_query(query) {
                for r in self.run_match(&or_query, limit)? {
                    if results.len() >= limit {
                        break;
                    }
                    if !results.iter().any(|existing| existing.path == r.path) {
                        results.push(r);
                    }
                }
            }
        }

        Ok(results)
    }

    /// Run one FTS5 MATCH over chunk rows and aggregate to file-level
    /// results: each file is scored by its best chunk.
    fn run_match(&self, match_expr: &str, limit: usize) -> Result<Vec<SearchResult>> {
        if match_expr.is_empty() {
            return Ok(vec![]);
        }
        // Path terms (directory names, filenames) are dense, descriptive
        // metadata; weighting them above body text helps needle queries in
        // large corpora. Env override exists for benchmark sweeps.
        let path_weight: f64 = std::env::var("HEARTHFS_PATH_WEIGHT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_PATH_WEIGHT);
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT path, snippet(fts_chunks, 1, '»', '«', '...', 40), bm25(fts_chunks, ?3, 1.0)
             FROM fts_chunks
             WHERE fts_chunks MATCH ?1
             ORDER BY bm25(fts_chunks, ?3, 1.0)
             LIMIT ?2",
        )?;

        // Over-fetch chunk rows so dedup still yields `limit` distinct files.
        let chunk_limit = (limit * 10) as i64;
        let mut results: Vec<SearchResult> = Vec::new();
        let rows = stmt.query_map(
            rusqlite::params![match_expr, chunk_limit, path_weight],
            |row| {
                Ok(SearchResult {
                    path: row.get(0)?,
                    snippet: row.get(1)?,
                    rank: row.get(2)?,
                })
            },
        )?;
        for row in rows {
            let row = row?;
            // Rows arrive best-first; keep only each file's best chunk.
            if !results.iter().any(|r| r.path == row.path) {
                results.push(row);
                if results.len() >= limit {
                    break;
                }
            }
        }

        Ok(results)
    }

    /// Check if a file is indexed with the given content hash.
    pub fn is_current(&self, path: &str, content_hash: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM indexed_files WHERE path = ?1 AND content_hash = ?2",
            [path, content_hash],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Get the stored token estimate for an indexed file.
    pub fn token_estimate(&self, path: &str) -> Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT token_estimate FROM indexed_files WHERE path = ?1")?;
        let mut rows = stmt.query([path])?;
        Ok(rows.next()?.map(|row| row.get(0)).transpose()?)
    }

    /// Get number of indexed files.
    pub fn file_count(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM indexed_files", [], |row| row.get(0))?;
        Ok(count)
    }
}

/// Sanitize a user query for FTS5.
/// Wraps terms containing special chars (hyphens, dots, etc.) in double quotes.
fn sanitize_fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(sanitize_term)
        .collect::<Vec<_>>()
        .join(" ")
}

fn sanitize_term(term: &str) -> String {
    if term.contains(|c: char| !c.is_alphanumeric() && c != '_') {
        format!("\"{}\"", term.replace('"', ""))
    } else {
        term.to_string()
    }
}

/// Build an OR query from the informative terms of a natural-language query.
/// Returns None if nothing survives stopword filtering.
fn or_fallback_query(query: &str) -> Option<String> {
    const STOPWORDS: &[&str] = &[
        "a", "an", "and", "are", "as", "at", "be", "but", "by", "did", "do", "does", "for", "from",
        "had", "has", "have", "how", "in", "is", "it", "its", "of", "on", "or", "that", "the",
        "their", "them", "then", "there", "these", "they", "this", "to", "was", "were", "what",
        "when", "where", "which", "who", "whom", "why", "will", "with",
    ];
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|t| t.len() > 1 && !STOPWORDS.contains(&t.to_lowercase().as_str()))
        .map(sanitize_term)
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" OR "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_index_and_search() {
        let idx = FtsIndex::open(Path::new(":memory:")).unwrap();
        idx.index_file(
            "src/main.rs",
            "fn main() { println!(\"hello\"); }",
            "abc123",
        )
        .unwrap();
        idx.index_file(
            "src/auth.rs",
            "fn authenticate(user: &str) { verify_token(user); }",
            "def456",
        )
        .unwrap();

        let results = idx.search("authenticate", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, "src/auth.rs");

        let results = idx.search("fn", 10).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_natural_language_query_falls_back_to_or() {
        let idx = FtsIndex::open(Path::new(":memory:")).unwrap();
        idx.index_file(
            "notes/invoice.md",
            "The Stitch invoice for April was $2,034, confirmed on the kickoff call.",
            "abc123",
        )
        .unwrap();

        // No single file contains every word of the question; the AND query
        // returns nothing and the OR fallback must find it.
        let results = idx
            .search(
                "What was the exact Stitch invoice amount stated by Devansh on the call?",
                10,
            )
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, "notes/invoice.md");
    }

    #[test]
    fn test_or_fallback_query_filters_stopwords() {
        let q = or_fallback_query("what was the invoice amount for April").unwrap();
        assert!(q.contains("invoice"));
        assert!(q.contains(" OR "));
        assert!(!q.to_lowercase().contains("\"what\""));
        assert!(or_fallback_query("the of and").is_none());
    }

    #[test]
    fn test_is_current() {
        let idx = FtsIndex::open(Path::new(":memory:")).unwrap();
        idx.index_file("foo.txt", "hello", "hash1").unwrap();
        assert!(idx.is_current("foo.txt", "hash1").unwrap());
        assert!(!idx.is_current("foo.txt", "hash2").unwrap());
    }

    #[test]
    fn test_reindex() {
        let idx = FtsIndex::open(Path::new(":memory:")).unwrap();
        idx.index_file("foo.txt", "old content", "hash1").unwrap();
        idx.index_file("foo.txt", "new content", "hash2").unwrap();

        let results = idx.search("old", 10).unwrap();
        assert_eq!(results.len(), 0);

        let results = idx.search("new", 10).unwrap();
        assert_eq!(results.len(), 1);
    }
}
