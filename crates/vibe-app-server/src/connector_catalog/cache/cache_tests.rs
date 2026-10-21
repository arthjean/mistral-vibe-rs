use serde_json::json;

use super::*;
use crate::connector_catalog::resolve_catalog;

#[test]
fn a_cached_catalog_round_trips_while_it_is_fresh() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(CACHE_FILE);
    let catalog = resolve_catalog(
        &json!({"connectors": [{"id": "x", "name": "X", "bootstrap_errors": "boom"}]}),
        "fp",
    )
    .unwrap();
    write_cache(&path, &catalog, 1_000).unwrap();
    let (read, stored_at) = read_cache(&path, "fp", 1_100).unwrap();
    assert_eq!((read, stored_at), (catalog, 1_000));
    assert!(read_cache(&path, "fp", 1_000 + CACHE_TTL_SECONDS).is_none());
    assert!(read_cache(&path, "other", 1_100).is_none());
}
