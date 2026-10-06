//! Development-only synthetic workload; not product capacity or HTTP benchmarking.
use std::{
    fs,
    sync::Arc,
    time::{Duration, Instant},
};

use puxiri::db::Database;
use rusqlite::{
    Connection, OpenFlags, TransactionBehavior,
    backup::{Backup, StepResult},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::oneshot;

pub async fn measure(issues: usize, objects: usize, writes: usize) -> Value {
    assert!(
        issues > 0 && issues <= 100_000 && objects <= 10_000 && writes <= 128 && writes <= issues
    );
    let directory = TempDir::new().unwrap();
    let database = Arc::new(Database::open(directory.path()).await.unwrap());
    let source_path = directory.path().join("puxiri.sqlite3");
    let seed_path = source_path.clone();
    tokio::task::spawn_blocking(move || {
        let mut connection = Connection::open(seed_path).unwrap();
        connection.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;
            CREATE TABLE probe_projects (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            CREATE TABLE probe_memberships (user INTEGER NOT NULL, project INTEGER NOT NULL REFERENCES probe_projects(id),
                PRIMARY KEY(user, project));
            CREATE TABLE probe_issues (id INTEGER PRIMARY KEY, project INTEGER NOT NULL REFERENCES probe_projects(id),
                title TEXT NOT NULL, body TEXT NOT NULL, status TEXT NOT NULL,
                revision INTEGER NOT NULL DEFAULT 0);
            CREATE INDEX probe_issues_by_project ON probe_issues(project, id);
            CREATE VIRTUAL TABLE probe_issue_fts USING fts5(title, body, tokenize='unicode61');
            CREATE TABLE probe_field_values (issue INTEGER NOT NULL REFERENCES probe_issues(id),
                field TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(issue, field));
            CREATE TABLE probe_comments (id INTEGER PRIMARY KEY, issue INTEGER NOT NULL REFERENCES probe_issues(id),
                body TEXT NOT NULL, created_at INTEGER NOT NULL);
            CREATE INDEX probe_comments_by_issue ON probe_comments(issue, id);
            CREATE TABLE probe_events (id INTEGER PRIMARY KEY, issue INTEGER NOT NULL REFERENCES probe_issues(id),
                kind TEXT NOT NULL, old_value TEXT, new_value TEXT, created_at INTEGER NOT NULL);
            CREATE INDEX probe_events_by_issue ON probe_events(issue, id);
            CREATE TABLE probe_objects (id INTEGER PRIMARY KEY);
            CREATE TABLE probe_refs (issue INTEGER PRIMARY KEY, object INTEGER NOT NULL REFERENCES probe_objects(id));
            CREATE TABLE probe_pins (run TEXT NOT NULL, object INTEGER NOT NULL REFERENCES probe_objects(id),
                PRIMARY KEY(run, object));").unwrap();
        let transaction = connection.transaction().unwrap();
        {
            let mut project = transaction
                .prepare("INSERT INTO probe_projects (id, name) VALUES (?, ?)")
                .unwrap();
            for id in 1..=20 {
                project
                    .execute((id, format!("Synthetic project {id}")))
                    .unwrap();
            }
            let mut membership = transaction
                .prepare("INSERT INTO probe_memberships (user, project) VALUES (1, ?)")
                .unwrap();
            for project_id in 1..=10 {
                membership.execute([project_id]).unwrap();
            }
            let mut issue = transaction.prepare("INSERT INTO probe_issues
                (id, project, title, body, status) VALUES (?, ?, ?, ?, ?)").unwrap();
            let mut fts = transaction.prepare("INSERT INTO probe_issue_fts (rowid, title, body) VALUES (?, ?, ?)").unwrap();
            let mut field = transaction
                .prepare("INSERT INTO probe_field_values (issue, field, value) VALUES (?, ?, ?)")
                .unwrap();
            let mut comment = transaction
                .prepare("INSERT INTO probe_comments (id, issue, body, created_at) VALUES (?, ?, ?, ?)")
                .unwrap();
            let mut event = transaction.prepare("INSERT INTO probe_events
                (id, issue, kind, old_value, new_value, created_at) VALUES (?, ?, ?, ?, ?, ?)").unwrap();
            let mut comment_id = 0_i64;
            let mut event_id = 0_i64;
            for id in 1..=issues {
                let project_id = (id % 20 + 1) as i64;
                let status = match id % 4 {
                    0 => "done",
                    1 => "backlog",
                    2 => "in_progress",
                    _ => "review",
                };
                let title = format!("Synthetic issue {id}");
                let body = format!("Issue {id} category{} description. {}", id % 100,
                    "Synthetic product description text. ".repeat(12));
                issue.execute((id as i64, project_id, &title, &body, status)).unwrap();
                fts.execute((id as i64, title, body)).unwrap();
                field.execute((id as i64, "priority", format!("P{}", id % 4))).unwrap();
                field.execute((id as i64, "estimate", (id % 13).to_string())).unwrap();
                if id % 3 == 0 {
                    comment_id += 1;
                    comment.execute((comment_id, id as i64,
                        format!("Comment for issue {id}. {}", "Synthetic discussion text. ".repeat(8)),
                        id as i64)).unwrap();
                }
                for ordinal in 0..2 {
                    event_id += 1;
                    event.execute((event_id, id as i64, "status_changed",
                        if ordinal == 0 { "backlog" } else { "in_progress" },
                        if ordinal == 0 { "in_progress" } else { status },
                        id as i64 * 2 + ordinal)).unwrap();
                }
            }
            let mut object = transaction.prepare("INSERT INTO probe_objects (id) VALUES (?)").unwrap();
            let mut reference = transaction.prepare("INSERT INTO probe_refs (issue, object) VALUES (?, ?)").unwrap();
            for id in 1..=objects {
                object.execute([id as i64]).unwrap();
                reference.execute((id as i64, id as i64)).unwrap();
            }
        }
        transaction.commit().unwrap();
    }).await.unwrap();
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .fetch_one(database.pool())
        .await
        .unwrap();
    let source_bytes = source_path.metadata().unwrap().len();
    let wal_path = directory.path().join("puxiri.sqlite3-wal");
    let baseline_wal = wal_path.metadata().unwrap().len();
    let destination_path = directory.path().join("snapshot.sqlite3");
    let verify_path = destination_path.clone();
    let (frozen_tx, frozen_rx) = oneshot::channel();
    let (writes_tx, writes_rx) = oneshot::channel();
    let snapshot = tokio::task::spawn_blocking(move || {
        let mut guard = Connection::open(&source_path).unwrap();
        guard
            .execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL")
            .unwrap();
        let freeze_start = Instant::now();
        let transaction = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let source =
            Connection::open_with_flags(source_path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        source.execute_batch("BEGIN").unwrap();
        let inventory_start = Instant::now();
        let inventory: Vec<i64> = source
            .prepare("SELECT DISTINCT object FROM probe_refs ORDER BY object")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let inventory_ms = inventory_start.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(inventory.len(), objects);
        {
            let mut insert = transaction
                .prepare("INSERT INTO probe_pins (run, object) VALUES ('probe', ?)")
                .unwrap();
            for id in &inventory {
                insert.execute([id]).unwrap();
            }
        }
        transaction.commit().unwrap();
        let freeze_ms = freeze_start.elapsed().as_secs_f64() * 1000.0;
        let reader_start = Instant::now();
        let mut destination = Connection::open(destination_path).unwrap();
        let copy_start = Instant::now();
        let copied_pages;
        {
            let backup = Backup::new(&source, &mut destination).unwrap();
            assert!(matches!(backup.step(1).unwrap(), StepResult::More));
            frozen_tx.send(()).unwrap();
            writes_rx.blocking_recv().unwrap();
            let deadline = Instant::now();
            loop {
                assert!(
                    deadline.elapsed() < Duration::from_secs(30),
                    "resource fixture exceeded its deadline"
                );
                match backup.step(128).unwrap() {
                    StepResult::Done => break,
                    StepResult::More => {}
                    StepResult::Busy | StepResult::Locked => {
                        std::thread::sleep(Duration::from_millis(1))
                    }
                    other => panic!("unexpected backup step: {other:?}"),
                }
            }
            copied_pages = backup.progress().pagecount;
        }
        let backup_including_writer_wait_ms = copy_start.elapsed().as_secs_f64() * 1000.0;
        source.execute_batch("COMMIT").unwrap();
        json!({"freeze_writer_lock_ms":freeze_ms,"inventory_ms":inventory_ms,
            "reader_hold_ms":reader_start.elapsed().as_secs_f64()*1000.0,
            "backup_including_writer_wait_ms":backup_including_writer_wait_ms,"copied_pages":copied_pages})
    });
    tokio::time::timeout(Duration::from_secs(30), frozen_rx)
        .await
        .unwrap()
        .unwrap();
    let mut latencies = Vec::new();
    for id in 1..=writes {
        let start = Instant::now();
        let mut transaction = database.pool().begin().await.unwrap();
        sqlx::query("UPDATE probe_issues SET revision = revision + 1, body = ? WHERE id = ?")
            .bind("writer fixture ".repeat(80))
            .bind(id as i64)
            .execute(&mut *transaction)
            .await
            .unwrap();
        transaction.commit().await.unwrap();
        latencies.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let held_wal = wal_path.metadata().unwrap().len();
    // TRUNCATE reports busy with a live old read transaction, not successful
    // disk reclamation. Bound the probe timeout instead of waiting five seconds.
    let checkpoint = {
        let path = directory.path().join("puxiri.sqlite3");
        tokio::task::spawn_blocking(move || {
            let connection = Connection::open(path).unwrap();
            connection.busy_timeout(Duration::from_millis(10)).unwrap();
            connection
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .unwrap()
        })
        .await
        .unwrap()
    };
    if writes > 0 {
        assert_eq!(checkpoint.0, 1);
        assert!(held_wal > baseline_wal);
    }
    writes_tx.send(()).unwrap();
    let snapshot_metrics = snapshot.await.unwrap();
    let snapshot_bytes = verify_path.metadata().unwrap().len();
    let verification = tokio::task::spawn_blocking(move || {
        let connection =
            Connection::open_with_flags(verify_path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let integrity: String = connection
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
        let count: i64 = connection
            .query_row("SELECT count(*) FROM probe_issues", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, issues as i64);
        let field_values: i64 = connection
            .query_row("SELECT count(*) FROM probe_field_values", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(field_values, (issues * 2) as i64);
        let comments: i64 = connection
            .query_row("SELECT count(*) FROM probe_comments", [], |row| row.get(0))
            .unwrap();
        assert_eq!(comments, (issues / 3) as i64);
        let events: i64 = connection
            .query_row("SELECT count(*) FROM probe_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(events, (issues * 2) as i64);
        let indexed: i64 = connection
            .query_row("SELECT count(*) FROM probe_issue_fts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(indexed, issues as i64);
        let memberships: i64 = connection
            .query_row("SELECT count(*) FROM probe_memberships", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(memberships, 10);
        let revisions: i64 = connection
            .query_row("SELECT sum(revision) FROM probe_issues", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(revisions, 0);
        let snapshot_pins: i64 = connection
            .query_row("SELECT count(*) FROM probe_pins", [], |row| row.get(0))
            .unwrap();
        assert_eq!(snapshot_pins, 0); // pins are deliberately committed after the frozen read.
    });
    verification.await.unwrap();
    let live_revisions: i64 = sqlx::query_scalar("SELECT sum(revision) FROM probe_issues")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(live_revisions, writes as i64);
    let search_path = directory.path().join("puxiri.sqlite3");
    let search = tokio::task::spawn_blocking(move || {
        let connection =
            Connection::open_with_flags(search_path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        connection.execute_batch("BEGIN").unwrap();
        let indexed_documents: i64 = connection
            .query_row("SELECT count(*) FROM probe_issue_fts", [], |row| row.get(0))
            .unwrap();
        let mut query = connection
            .prepare(
                "SELECT issue.project
                 FROM probe_issue_fts
                 JOIN probe_issues AS issue ON issue.id = probe_issue_fts.rowid
                 JOIN probe_memberships AS membership
                   ON membership.project = issue.project AND membership.user = ?1
                 WHERE probe_issue_fts MATCH ?2
                 ORDER BY bm25(probe_issue_fts), issue.id
                 LIMIT ?3",
            )
            .unwrap();
        let mut timings = Vec::with_capacity(50);
        let mut returned_rows = 0_usize;
        let mut max_rows_per_query = 0_usize;
        let mut unauthorized_rows = 0_usize;
        for index in 0..50 {
            let term = format!("category{}", index * 2);
            let start = Instant::now();
            let projects = query
                .query_map((1_i64, term, 100_i64), |row| row.get::<_, i64>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            timings.push(start.elapsed().as_secs_f64() * 1000.0);
            returned_rows += projects.len();
            max_rows_per_query = max_rows_per_query.max(projects.len());
            unauthorized_rows += projects
                .iter()
                .filter(|project| !(1..=10).contains(*project))
                .count();
        }
        timings.sort_by(f64::total_cmp);
        let p95_ms = timings[(timings.len() * 95).div_ceil(100).saturating_sub(1)];
        connection.execute_batch("COMMIT").unwrap();
        json!({"query_count":timings.len(), "indexed_documents":indexed_documents,
            "returned_rows":returned_rows, "unauthorized_rows":unauthorized_rows,
            "max_rows_per_query":max_rows_per_query,"query_p95_ms":p95_ms,
            "query_total_ms":timings.iter().sum::<f64>()})
    })
    .await
    .unwrap();
    let after: (i64, i64, i64) = sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(after, (0, 0, 0));
    let released_wal = wal_path.metadata().unwrap().len();
    assert_eq!(released_wal, 0);
    latencies.sort_by(f64::total_cmp);
    let p95 = latencies
        .get((latencies.len() * 95).div_ceil(100).saturating_sub(1))
        .copied();
    let report = json!({"workload":"synthetic SQLite snapshot/pins/search with projects, custom fields, comments and history; NOT product capacity",
        "issues":issues,"projects":20,"field_values":issues * 2,"comments":issues / 3,
        "events":issues * 2,"memberships":10,"fts_documents":issues,"search":search,
        "objects":objects,"commits_while_reader_held":writes,
        "source_db_bytes":source_bytes,"wal_baseline_bytes":baseline_wal,"wal_held_bytes":held_wal,
        "snapshot_db_bytes":snapshot_bytes,
        "wal_after_reader_release_bytes":released_wal,"held_checkpoint":{"busy":checkpoint.0,
            "log_frames":checkpoint.1,"checkpointed_frames":checkpoint.2},
        "write_transaction_ms":{"samples":latencies,"p95":p95},"snapshot":snapshot_metrics,
        "proc_status":fs::read_to_string("/proc/self/status").ok().map(|text| text.lines()
            .filter(|line| line.starts_with("VmRSS:") || line.starts_with("VmHWM:") || line.starts_with("Threads:"))
            .collect::<Vec<_>>().join("\n")),
        "cgroup_cpu_max":fs::read_to_string("/sys/fs/cgroup/cpu.max").ok(),
        "cgroup_memory_max":fs::read_to_string("/sys/fs/cgroup/memory.max").ok(),
        "cgroup_memory_peak":fs::read_to_string("/sys/fs/cgroup/memory.peak").ok(),
        "cgroup_cpu_stat":fs::read_to_string("/sys/fs/cgroup/cpu.stat").ok()});
    database.close().await;
    report
}
