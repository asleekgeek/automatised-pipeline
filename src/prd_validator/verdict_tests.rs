use super::*;
use crate::graph_store::NODE_FILE;
use std::fs;

// source: review finding #3 on issue #13's fix — file_has_graph_node used
// to build its Cypher literal via `.replace('\'', "\\'")`, which a
// `\'` payload in claim.token (LLM-generated PRD content, not trusted)
// can defeat: the escape turns `\'` into `\\'`, an escaped backslash
// followed by an UNescaped closing quote, breaking out of the string
// literal early. Mirrors graph_store::tests::test_cypher_injection_rejected.
#[test]
fn test_file_has_graph_node_escapes_adversarial_path() {
    // GraphStore persists as a single-file embedded db, not a directory
    // (source: main.rs::remove_stale_graph_artifact / the ENOTDIR fix it
    // documents). `remove_dir_all` on that file fails silently (ignored
    // errors below would otherwise leak the db across test runs and
    // cause spurious "duplicated primary key" failures on rerun) — so,
    // matching graph_store::tests::test_cypher_injection_rejected, the
    // db lives inside a wrapping directory that IS safe to remove_dir_all.
    let dir = std::env::temp_dir().join("prd_validator_cypher_inject_test");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    let db_path = dir.join("testdb");
    let store = GraphStore::open_or_create(&db_path).expect("open_or_create");
    store.create_schema().expect("create_schema");

    // Adversarial path: a literal `\'` sequence followed by a Cypher
    // payload. Under the old naive escape this closes the string early
    // and lets `DETACH DELETE n` execute as live Cypher.
    let evil_path = r"weird\'.rs' DETACH DELETE n //";
    store
        .insert_node(
            NODE_FILE,
            &[
                ("id", &cypher_str(evil_path)),
                ("path", &cypher_str(evil_path)),
                ("name", &cypher_str("evil.rs")),
                ("extension", &cypher_str("rs")),
                ("size_bytes", "0"),
            ],
        )
        .expect("insert adversarial file node");

    // A second, benign node — if the adversarial path's DETACH DELETE
    // had executed, this node would vanish too.
    store
        .insert_node(
            NODE_FILE,
            &[
                ("id", &cypher_str("safe.rs")),
                ("path", &cypher_str("safe.rs")),
                ("name", &cypher_str("safe.rs")),
                ("extension", &cypher_str("rs")),
                ("size_bytes", "0"),
            ],
        )
        .expect("insert safe file node");

    assert!(
        file_has_graph_node(&store, evil_path),
        "the adversarial path must still round-trip as an ordinary string and match its own node"
    );
    assert!(
        file_has_graph_node(&store, "safe.rs"),
        "the benign node must survive — the adversarial path must not have executed DETACH DELETE"
    );

    let _ = fs::remove_dir_all(&dir);
}
