use chrono::{TimeDelta, Utc};
use puxiri::{
    auth::{
        SessionToken,
        session::{SessionError, SessionListOptions, SessionPolicy, SessionStore},
    },
    db::Database,
};
use sqlx::Row;
use tempfile::TempDir;
use uuid::Uuid;

fn options(cursor: Option<&str>, size: u16) -> SessionListOptions<'_> {
    SessionListOptions {
        after_session_id: cursor,
        page_size: size,
    }
}

async fn user(database: &Database) -> String {
    let id = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id, display_name, state, created_at, updated_at) VALUES (?, 'Session list', 'active', 'now', 'now')")
        .bind(&id).execute(database.pool()).await.unwrap();
    id
}

#[tokio::test]
async fn bounded_candidate_pages_filter_invalid_rows_without_losing_continuation_or_ownership() {
    let directory = TempDir::new().unwrap();
    let db = Database::open(directory.path()).await.unwrap();
    let actor = user(&db).await;
    let stranger = user(&db).await;
    let store = SessionStore::new(
        db.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    );
    let now = Utc::now();
    let own = store.issue(&actor, now).await.unwrap();
    let other = store.issue(&actor, now).await.unwrap();
    let foreign = store.issue(&stranger, now).await.unwrap();
    let expired = store.issue(&actor, now - TimeDelta::days(8)).await.unwrap();
    let idle = store.issue(&actor, now - TimeDelta::days(1)).await.unwrap();
    let future = store
        .issue(&actor, now + TimeDelta::seconds(1))
        .await
        .unwrap();
    let malformed = store.issue(&actor, now).await.unwrap();
    let revoked = store.issue(&actor, now).await.unwrap();
    store.revoke(&actor, &revoked.id, now).await.unwrap();
    sqlx::query("UPDATE sessions SET last_activity_at = 'malformed' WHERE id = ?")
        .bind(&malformed.id)
        .execute(db.pool())
        .await
        .unwrap();
    let mut cursor = None;
    let mut found = Vec::new();
    let mut empty_pages = 0;
    let mut page_count = 0;
    loop {
        let page = store
            .list_owned(&own.token, options(cursor.as_deref(), 1), now)
            .await
            .unwrap()
            .unwrap();
        page_count += 1;
        assert!(page_count <= 6, "cursor must make bounded forward progress");
        assert!(page.items.len() <= 1);
        if page.items.is_empty() {
            empty_pages += 1;
        }
        for summary in page.items {
            assert_eq!(summary.is_current, summary.id == own.id);
            found.push(summary.id);
        }
        match page.next_after_session_id {
            Some(next) => {
                if let Some(previous) = cursor {
                    assert!(next < previous);
                }
                cursor = Some(next);
            }
            None => break,
        }
    }
    let mut expected = vec![own.id.clone(), other.id.clone()];
    expected.sort_by(|a, b| b.cmp(a));
    assert_eq!(found, expected);
    assert_eq!(page_count, 6);
    assert_eq!(empty_pages, 4);
    for id in [
        &expired.id,
        &idle.id,
        &future.id,
        &malformed.id,
        &revoked.id,
        &foreign.id,
    ] {
        assert!(!found.contains(id));
    }
    // Arbitrary/foreign boundaries are not lookups or grants of authority.
    let page = store
        .list_owned(&own.token, options(Some(&foreign.id), 100), now)
        .await
        .unwrap()
        .unwrap();
    for summary in page.items {
        assert!(expected.contains(&summary.id));
        assert!(summary.id < foreign.id);
    }
    let first = store
        .list_owned(&own.token, options(None, 1), now)
        .await
        .unwrap()
        .unwrap();
    let boundary = first.next_after_session_id.unwrap();
    // Removing even an invalid candidate boundary cannot skip its successor.
    sqlx::query("DELETE FROM sessions WHERE id = ?")
        .bind(&boundary)
        .execute(db.pool())
        .await
        .unwrap();
    let continuation = store
        .list_owned(&own.token, options(Some(&boundary), 100), now)
        .await
        .unwrap()
        .unwrap();
    for summary in continuation.items {
        assert!(summary.id < boundary);
    }
}

#[tokio::test]
async fn session_listing_revalidates_identity_deadlines_and_never_touches_activity() {
    let directory = TempDir::new().unwrap();
    let db = Database::open(directory.path()).await.unwrap();
    let actor = user(&db).await;
    let store = SessionStore::new(
        db.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    );
    let now = Utc::now();
    let own = store.issue(&actor, now).await.unwrap();
    let before: String = sqlx::query_scalar("SELECT last_activity_at FROM sessions WHERE id = ?")
        .bind(&own.id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(
        store
            .list_owned(&own.token, options(None, 100), now + TimeDelta::minutes(10))
            .await
            .unwrap()
            .is_some()
    );
    let after: String = sqlx::query_scalar("SELECT last_activity_at FROM sessions WHERE id = ?")
        .bind(&own.id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(before, after);
    for instant in [
        now - TimeDelta::seconds(1),
        now + TimeDelta::hours(24),
        now + TimeDelta::days(7),
    ] {
        assert!(
            store
                .list_owned(&own.token, options(None, 50), instant)
                .await
                .unwrap()
                .is_none()
        );
    }
    assert!(
        store
            .list_owned(&SessionToken::generate().unwrap(), options(None, 50), now)
            .await
            .unwrap()
            .is_none()
    );
    sqlx::query("UPDATE users SET state = 'banned' WHERE id = ?")
        .bind(&actor)
        .execute(db.pool())
        .await
        .unwrap();
    assert!(
        store
            .list_owned(&own.token, options(None, 50), now)
            .await
            .unwrap()
            .is_none()
    );
    sqlx::query("UPDATE users SET state = 'active' WHERE id = ?")
        .bind(&actor)
        .execute(db.pool())
        .await
        .unwrap();
    store.revoke(&actor, &own.id, now).await.unwrap();
    assert!(
        store
            .list_owned(&own.token, options(None, 50), now)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn exact_maximum_page_and_lookahead_cover_all_records_without_duplicates() {
    let directory = TempDir::new().unwrap();
    let db = Database::open(directory.path()).await.unwrap();
    let actor = user(&db).await;
    let store = SessionStore::new(
        db.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    );
    let now = Utc::now();
    let own = store.issue(&actor, now).await.unwrap();
    let mut expected = vec![own.id.clone()];
    for _ in 0..100 {
        expected.push(store.issue(&actor, now).await.unwrap().id);
    }
    expected.sort_by(|a, b| b.cmp(a));
    let first = store
        .list_owned(&own.token, options(None, 100), now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.items.len(), 100);
    let cursor = first.next_after_session_id.unwrap();
    let last = store
        .list_owned(&own.token, options(Some(&cursor), 100), now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(last.items.len(), 1);
    assert!(last.next_after_session_id.is_none());
    let mut actual = Vec::new();
    for summary in first.items.into_iter().chain(last.items) {
        actual.push(summary.id);
    }
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn listing_validates_bounds_uses_keyset_index_and_reports_corrupt_identity_or_storage() {
    let directory = TempDir::new().unwrap();
    let db = Database::open(directory.path()).await.unwrap();
    let actor = user(&db).await;
    let store = SessionStore::new(
        db.pool().clone(),
        SessionPolicy::initial(TimeDelta::minutes(5)).unwrap(),
    );
    let now = Utc::now();
    let own = store.issue(&actor, now).await.unwrap();
    for size in [0, 101, u16::MAX] {
        assert!(matches!(
            store.list_owned(&own.token, options(None, size), now).await,
            Err(SessionError::InvalidPageSize)
        ));
    }
    for cursor in [
        "",
        "garbage",
        "00000000000000000000000000000000",
        "FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF",
    ] {
        assert!(matches!(
            store
                .list_owned(&own.token, options(Some(cursor), 1), now)
                .await,
            Err(SessionError::InvalidCursor)
        ));
    }
    let empty = store
        .list_owned(
            &own.token,
            options(Some("00000000-0000-0000-0000-000000000000"), 100),
            now,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(empty.items.is_empty());
    assert!(empty.next_after_session_id.is_none());
    let plan = sqlx::query("EXPLAIN QUERY PLAN SELECT sessions.id, sessions.user_id, sessions.created_at, sessions.last_activity_at, sessions.expires_at, users.state FROM sessions JOIN users ON users.id = sessions.user_id WHERE sessions.user_id = ? AND sessions.revoked_at IS NULL AND sessions.id < ? ORDER BY sessions.id DESC LIMIT ?")
        .bind(&actor).bind("~").bind(101_i64).fetch_all(db.pool()).await.unwrap();
    let details: Vec<String> = plan.iter().map(|row| row.get("detail")).collect();
    assert!(
        details
            .iter()
            .any(|detail| detail.contains("sessions_user_id_keyset"))
    );
    assert!(!details.iter().any(|detail| detail.contains("TEMP B-TREE")));
    let corrupt = store.issue(&actor, now).await.unwrap();
    sqlx::query("UPDATE sessions SET id = 'gggggggg-gggg-gggg-gggg-gggggggggggg' WHERE id = ?")
        .bind(&corrupt.id)
        .execute(db.pool())
        .await
        .unwrap();
    assert!(matches!(
        store.list_owned(&own.token, options(None, 100), now).await,
        Err(SessionError::InvalidSessionData)
    ));
    db.pool().close().await;
    assert!(matches!(
        store.list_owned(&own.token, options(None, 100), now).await,
        Err(SessionError::Database(_))
    ));
}
