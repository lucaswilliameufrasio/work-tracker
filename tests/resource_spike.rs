#[path = "support/resource_probe.rs"]
mod resource_probe;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_snapshot_retains_real_wal_until_reader_release_then_truncates() {
    let report = resource_probe::measure(2_000, 200, 32).await;
    assert_eq!(report["held_checkpoint"]["busy"], 1);
    assert_eq!(report["wal_after_reader_release_bytes"], 0);
    assert_eq!(report["commits_while_reader_held"], 32);
    assert_eq!(report["projects"], 20);
    assert_eq!(report["field_values"], 4_000);
    assert_eq!(report["comments"], 666);
    assert_eq!(report["events"], 4_000);
    assert_eq!(report["search"]["query_count"], 50);
    assert_eq!(report["search"]["indexed_documents"], 2_000);
    assert_eq!(report["search"]["unauthorized_rows"], 0);
    assert!(report["search"]["max_rows_per_query"].as_u64().unwrap() <= 100);
    assert!(report["search"]["returned_rows"].as_u64().unwrap() > 0);
}
