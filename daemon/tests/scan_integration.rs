//! End-to-end contradiction-scanner tests against a real Postgres (pgvector).
//! Skipped without DATABASE_URL, like the other integration suites.
//!
//! Flow: ingest two chat exports asserting conflicting numeric facts, drain
//! extraction, run the scanner, and verify detection → review → resolution.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::Row;
use tower::ServiceExt;
use uuid::Uuid;

use gather_daemon::config::Config;
use gather_daemon::{db, extract, routes, scan, AppState};

async fn test_state() -> Option<AppState> {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping integration test: DATABASE_URL not set");
        return None;
    };
    let pool = db::connect(&database_url).await.expect("db connect");
    db::migrate(&pool).await.expect("migrations");
    Some(AppState {
        pool,
        config: Arc::new(Config::for_tests(database_url)),
        metrics: metrics_exporter_prometheus::PrometheusBuilder::new()
            .build_recorder()
            .handle(),
    })
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn ingest_message(app: &axum::Router, conversation_id: &str, content: &str) {
    let export = json!({
        "platform": "generic",
        "data": {
            "schema": "gather-generic-v1",
            "conversations": [{
                "id": conversation_id,
                "messages": [
                    {"role": "user", "content": content, "created_at": "2026-04-01T10:00:00Z"}
                ]
            }]
        }
    });
    let res = app
        .clone()
        .oneshot(
            Request::post("/api/v1/ingest/chat-export")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(export.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
}

/// Drain extraction then the scanner, waiting on queue state (not pass
/// counts) because integration tests run in parallel.
async fn drain_all(state: &AppState) {
    for _ in 0..200 {
        extract::run_one_pass(&state.pool, &state.config, None)
            .await
            .expect("extraction pass");
        scan::run_one_scan(&state.pool, &state.config, None)
            .await
            .expect("scan pass");
        let (busy,): (i64,) = sqlx::query_as(
            r#"
            SELECT
              (SELECT count(*) FROM documents
               WHERE extraction_status IN ('pending','processing'))
            + (SELECT count(*) FROM images
               WHERE ocr_status IN ('pending','processing'))
            + (SELECT count(*) FROM messages WHERE units_extracted_at IS NULL)
            + (SELECT count(*) FROM document_segments WHERE units_extracted_at IS NULL)
            + (SELECT count(*) FROM atomic_units
               WHERE contradiction_scanned_at IS NULL AND status = 'active')
            "#,
        )
        .fetch_one(&state.pool)
        .await
        .expect("queue state query");
        if busy == 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("queues did not drain within the deadline");
}

#[tokio::test]
async fn conflicting_numeric_facts_are_detected_and_resolvable() {
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());

    // A unique subject keeps this run isolated from other test data; both
    // statements resolve to the same subject entity via rules.rs.
    let marker = &Uuid::new_v4().simple().to_string()[..8];
    let subject = format!("Zeta{marker} budget");
    ingest_message(
        &app,
        &format!("scan-{marker}-a"),
        &format!("My {subject} is $50 per month."),
    )
    .await;
    ingest_message(
        &app,
        &format!("scan-{marker}-b"),
        &format!("My {subject} is $75 per month."),
    )
    .await;

    drain_all(&state).await;

    // Exactly one open contradiction between the two units.
    let rows = sqlx::query(
        r#"
        SELECT c.id, c.score, c.detection_method, c.status::text AS status,
               a.statement AS statement_a, b.statement AS statement_b
        FROM contradictions c
        JOIN atomic_units a ON a.id = c.unit_a_id
        JOIN atomic_units b ON b.id = c.unit_b_id
        WHERE a.statement LIKE '%' || $1 || '%' AND b.statement LIKE '%' || $1 || '%'
        "#,
    )
    .bind(&subject)
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1, "expected exactly one detected contradiction");
    let row = &rows[0];
    let contradiction_id: Uuid = row.get("id");
    assert_eq!(row.get::<String, _>("status"), "open");
    assert_eq!(
        row.get::<String, _>("detection_method"),
        "rule:numeric-mismatch"
    );
    assert!(row.get::<f32, _>("score") >= 0.65);

    // Detection left an audit row from the scanner.
    let (audit_count,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM contradiction_audit
         WHERE contradiction_id = $1 AND action = 'detected' AND actor = 'scanner'",
    )
    .bind(contradiction_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(audit_count, 1);

    // Re-scanning does not duplicate the pair.
    sqlx::query("UPDATE atomic_units SET contradiction_scanned_at = NULL WHERE statement LIKE '%' || $1 || '%'")
        .bind(&subject)
        .execute(&state.pool)
        .await
        .unwrap();
    drain_all(&state).await;
    let (pair_count,): (i64,) = sqlx::query_as(
        r#"SELECT count(*) FROM contradictions c
           JOIN atomic_units a ON a.id = c.unit_a_id
           WHERE a.statement LIKE '%' || $1 || '%'"#,
    )
    .bind(&subject)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(pair_count, 1, "re-scan must not duplicate the pair");

    // The REST review surface sees it, and resolution propagates (§6.3).
    // unit_a/unit_b ordering is by UUID, so pick the side that keeps $75.
    let resolution = if row.get::<String, _>("statement_a").contains("$75") {
        "resolved_a"
    } else {
        "resolved_b"
    };
    let res = app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/contradictions/{contradiction_id}/resolve"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"resolution": resolution, "note": "newer figure wins"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let resolved = body_json(res).await;
    assert_eq!(resolved["status"], json!(resolution));

    let (loser_status,): (String,) = sqlx::query_as(
        r#"SELECT status::text FROM atomic_units
           WHERE statement LIKE '%' || $1 || '%' AND statement LIKE '%$50%'"#,
    )
    .bind(&subject)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(loser_status, "superseded");
}

#[tokio::test]
async fn negation_is_detected_and_agreement_is_not() {
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());

    let marker = &Uuid::new_v4().simple().to_string()[..8];
    // Conflicting pair: plain assertion vs negation of the same content.
    ingest_message(
        &app,
        &format!("neg-{marker}-a"),
        &format!("I use Vectron{marker} for search."),
    )
    .await;
    ingest_message(
        &app,
        &format!("neg-{marker}-b"),
        &format!("I never use Vectron{marker} for search."),
    )
    .await;
    // Agreeing pair: same tool asserted twice differently — no contradiction.
    ingest_message(
        &app,
        &format!("agr-{marker}-a"),
        &format!("I use Quorix{marker} for backups."),
    )
    .await;
    ingest_message(
        &app,
        &format!("agr-{marker}-b"),
        &format!("I have Quorix{marker} for the backup jobs."),
    )
    .await;

    drain_all(&state).await;

    let count = |needle: String, state: AppState| async move {
        let (n,): (i64,) = sqlx::query_as(
            r#"SELECT count(*) FROM contradictions c
               JOIN atomic_units a ON a.id = c.unit_a_id
               JOIN atomic_units b ON b.id = c.unit_b_id
               WHERE a.statement LIKE '%' || $1 || '%' AND b.statement LIKE '%' || $1 || '%'"#,
        )
        .bind(&needle)
        .fetch_one(&state.pool)
        .await
        .unwrap();
        n
    };
    assert_eq!(
        count(format!("Vectron{marker}"), state.clone()).await,
        1,
        "negation pair must be detected"
    );
    assert_eq!(
        count(format!("Quorix{marker}"), state.clone()).await,
        0,
        "agreeing statements must not be flagged"
    );
}
