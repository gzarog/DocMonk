//! The canonical contract: required root fields, round trip, and no
//! field name from any earlier status shape.

use docmonk_status::model::{HealthState, Liveness, Mode, StatusReport};

const REQUIRED_ROOT: [&str; 12] = [
    "schema_version",
    "snapshot_id",
    "observed_at",
    "mode",
    "backend",
    "health",
    "indexer",
    "summary",
    "sources",
    "problems",
    "recent_errors",
    "diagnostics",
];

fn fixture() -> serde_json::Value {
    serde_json::from_str(include_str!("../fixtures/server_multi_host.json")).unwrap()
}

#[test]
fn fixture_round_trips_with_exact_root_fields() {
    let raw = fixture();
    let report: StatusReport = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(report.schema_version, docmonk_status::SCHEMA_VERSION);
    assert_eq!(report.mode, Mode::Server);
    assert_eq!(report.health.state, HealthState::Degraded);
    let hosts: Vec<_> = report
        .indexer
        .workers
        .iter()
        .map(|w| (w.host.as_deref(), w.liveness))
        .collect();
    assert_eq!(
        hosts,
        [
            (Some("host-a"), Liveness::Live),
            (Some("host-b"), Liveness::Live)
        ]
    );
    let back = serde_json::to_value(&report).unwrap();
    assert_eq!(back, raw, "serialization is lossless");
    let keys: Vec<&str> = back
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let mut want = REQUIRED_ROOT.to_vec();
    want.sort_unstable();
    let mut got = keys.clone();
    got.sort_unstable();
    assert_eq!(got, want);
}

#[test]
fn earlier_field_names_are_absent() {
    let text = serde_json::to_string(&fixture()).unwrap();
    for old in [
        "\"totals\"",
        "\"by_status\"",
        "\"queue_depth\"",
        "\"current_source_id\"",
        "\"database_size_bytes\"",
        "\"tokenizer\"",
        "\"recent_error_count\"",
        "\"sources_with_errors\"",
    ] {
        assert!(
            !text.contains(old),
            "{old} must not be part of the contract"
        );
    }
}
