//! Contract test: the API must match the committed `openapi.json` at the repository root.
//! Clients use hand-written DTOs, so every API change shows up here as a reviewed diff.
//! Regenerate with `BERTH_UPDATE_SNAPSHOT=1 cargo test --test openapi_snapshot`.

use std::path::Path;

#[test]
fn openapi_matches_snapshot() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../openapi.json");
    let current = berth_server::openapi().to_pretty_json().unwrap() + "\n";
    if std::env::var_os("BERTH_UPDATE_SNAPSHOT").is_some() {
        std::fs::write(&path, &current).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(&path)
        .expect("openapi.json missing: run with BERTH_UPDATE_SNAPSHOT=1")
        .replace("\r\n", "\n");
    assert!(
        committed == current,
        "the API changed: review it and run with BERTH_UPDATE_SNAPSHOT=1 to update openapi.json"
    );
}
