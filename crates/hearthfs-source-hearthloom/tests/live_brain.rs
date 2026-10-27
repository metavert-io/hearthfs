//! Live integration test against a local Hearthloom daemon.
//!
//! Exercises the same Source layer an NFS/FSKit mount serves: root listing,
//! BRAIN.md read, collection listing/round-trip reads, server-side @fit
//! (?budget=), and delegated search. A full kernel NFS mount is deliberately
//! not attempted in-suite (it needs a real mount); this covers everything
//! above the transport.
//!
//! SKIPS (passes vacuously, with a note) when no key is stored or no daemon
//! is listening — so `cargo test` stays green on machines without a brain.

use std::path::Path;

use hearthfs_core::source::Source;
use hearthfs_source_hearthloom::{HearthloomSource, DEFAULT_BASE_URL, ORIGIN_MARKER};

async fn connect_or_skip() -> Option<HearthloomSource> {
    let Some(key) = HearthloomSource::load_key() else {
        eprintln!("SKIP: no key at ~/.config/hearthfs/hearthloom.key (run: hearthfs auth hearthloom)");
        return None;
    };
    match HearthloomSource::connect(DEFAULT_BASE_URL, &key).await {
        Ok(src) => Some(src),
        Err(e) => {
            eprintln!("SKIP: hearthloom daemon not reachable at {DEFAULT_BASE_URL}: {e}");
            None
        }
    }
}

#[tokio::test]
async fn brain_mount_surface() {
    let Some(src) = connect_or_skip().await else {
        return;
    };

    // The brain owns its own index — mounts must never re-embed it.
    assert!(src.skip_indexing(), "brain sources must skip local indexing");
    assert!(src.supports_search(), "manifest must advertise search");

    // Root: the tree the handoff doc promises.
    let root = src.list(Path::new("")).await.expect("list root");
    let names: Vec<&str> = root.iter().map(|e| e.name.as_str()).collect();
    for expected in ["BRAIN.md", ORIGIN_MARKER, "entities", "days", "goals"] {
        assert!(
            names.contains(&expected),
            "root listing missing {expected}; got {names:?}"
        );
    }

    // BRAIN.md: the "read this first" index, real content.
    let brain = src.read(Path::new("BRAIN.md")).await.expect("read BRAIN.md");
    let brain_text = String::from_utf8_lossy(&brain);
    assert!(
        brain_text.contains("BRAIN.md") || brain_text.contains("Hearthloom"),
        "BRAIN.md doesn't look like the brain index"
    );

    // Entities collection: names must be unique, non-empty, .md-suffixed —
    // duplicates or empties corrupt NFS READDIR replies (live bug 2026-07-11).
    let entities = src.list(Path::new("entities")).await.expect("list entities");
    assert!(!entities.is_empty(), "entities collection came back empty");
    let mut seen = std::collections::HashSet::new();
    for e in &entities {
        assert!(e.name.ends_with(".md"), "entity entry without .md: {}", e.name);
        assert!(e.name.len() > 3, "empty entity slug: {:?}", e.name);
        assert!(seen.insert(e.name.clone()), "duplicate entity entry: {}", e.name);
    }

    // Round-trip: every served name must be readable back (slug → id).
    let first = &entities[0].name;
    let dossier = src
        .read(Path::new(&format!("entities/{first}")))
        .await
        .unwrap_or_else(|e| panic!("read entities/{first}: {e}"));
    assert!(!dossier.is_empty(), "empty dossier for {first}");

    // Server-side @fit: ?budget= must yield something, and something smaller.
    let full = src.read(Path::new("BRAIN.md")).await.expect("full read");
    let fitted = src
        .read_fit(Path::new("BRAIN.md"), 500)
        .await
        .expect("read_fit")
        .expect("smart source must serve budget-shaped reads");
    assert!(!fitted.is_empty(), "budget read came back empty");
    assert!(
        fitted.len() < full.len(),
        "budget=500 ({} bytes) not smaller than full ({} bytes)",
        fitted.len(),
        full.len()
    );

    // Delegated search: scored results with paths that resolve in the tree.
    let hits = src.search("hearthloom", 5).await.expect("search");
    assert!(!hits.is_empty(), "search returned nothing for 'hearthloom'");
}
