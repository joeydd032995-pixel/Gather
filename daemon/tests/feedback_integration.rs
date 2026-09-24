//! End-to-end tests for the feedback loop (autonomous pipeline, Phase A):
//! reversible reject/restore, confirm, edit, and the optional review tray.
//! Skipped when DATABASE_URL is unset, like the other integration suites.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::Row;
use tower::ServiceExt;
use uuid::Uuid;

use gather_daemon::config::Config;
use gather_daemon::{db, routes, AppState};

async fn test_state() -> Option<AppState> {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping integration test: DATABASE_URL not set");
        return None;
    };
    let pool = db::connect(&database_url).await.expect("db connect");
    db::migrate(&pool).await.expect("migrations");
    let config = Config::for_tests(database_url);
    Some(AppState {
        pool,
        config: Arc::new(config),
        metrics: metrics_exporter_prometheus::PrometheusBuilder::new()
            .build_recorder()
            .handle(),
        ollama: None,
        rate_limiter: None,
    })
}

/// Insert a bare atomic unit and return its id. Unique statement hash keeps
/// repeated test runs from colliding on the dedup constraint.
async fn seed_unit(state: &AppState, statement: &str) -> Uuid {
    let hash = format!("test-{}", Uuid::new_v4());
    let row = sqlx::query(
        "INSERT INTO atomic_units (kind, statement, statement_hash, confidence, extraction_method) \
         VALUES ('fact', $1, $2, 0.6, 'rule_based') RETURNING id",
    )
    .bind(statement)
    .bind(&hash)
    .fetch_one(&state.pool)
    .await
    .expect("seed unit");
    row.get("id")
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn unit_status(state: &AppState, id: Uuid) -> String {
    sqlx::query_scalar::<_, String>("SELECT status::text FROM atomic_units WHERE id = $1")
        .bind(id)
        .fetch_one(&state.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn reject_retracts_and_restore_reverses_it() {
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());
    let id = seed_unit(&state, "reject me").await;

    // reject -> retracted
    let res = app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/units/{id}/reject"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(unit_status(&state, id).await, "retracted");

    // a negative-label feedback row was written
    let rejects: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM unit_feedback WHERE target_id = $1 AND action = 'reject'",
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(rejects, 1);

    // restore -> active again (the undo path)
    let res = app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/units/{id}/restore"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(unit_status(&state, id).await, "active");
}

#[tokio::test]
async fn restore_rejects_a_non_retracted_unit() {
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());
    let id = seed_unit(&state, "active, never rejected").await;
    // Restoring an active (or superseded) unit must be refused, so it can't
    // strip contradiction lifecycle metadata.
    let res = app
        .oneshot(
            Request::post(format!("/api/v1/units/{id}/restore"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_eq!(unit_status(&state, id).await, "active");
}

#[tokio::test]
async fn rejecting_resolves_the_open_review_entry() {
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());
    let id = seed_unit(&state, "parked then rejected").await;
    sqlx::query(
        "INSERT INTO review_queue (target_kind, target_id, reason) VALUES ('unit', $1, 'low-confidence')",
    )
    .bind(id)
    .execute(&state.pool)
    .await
    .unwrap();

    // tray shows the parked unit
    let res = app
        .clone()
        .oneshot(Request::get("/api/v1/review").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let json = body_json(res).await;
    let listed = json["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["target_id"] == json!(id.to_string()));
    assert!(listed, "seeded unit should appear in the review tray");

    // acting on the unit drains its tray entry
    app.clone()
        .oneshot(
            Request::post(format!("/api/v1/units/{id}/reject"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM review_queue WHERE target_id = $1 AND state = 'open'",
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(open, 0, "reject should resolve the open review entry");
}

#[tokio::test]
async fn edit_rewrites_statement_and_records_correction() {
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());
    let id = seed_unit(&state, "original wording").await;
    // Unique per run so repeated runs against a persistent DB don't collide on
    // the recomputed statement hash.
    let corrected = format!("corrected wording {}", Uuid::new_v4());

    let res = app
        .clone()
        .oneshot(
            Request::patch(format!("/api/v1/units/{id}"))
                .header("content-type", "application/json")
                .body(Body::from(json!({ "statement": corrected }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let (statement, method): (String, String) =
        sqlx::query_as("SELECT statement, extraction_method::text FROM atomic_units WHERE id = $1")
            .bind(id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(statement, corrected);
    assert_eq!(method, "manual");

    let edits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM unit_feedback WHERE target_id = $1 AND action = 'edit'",
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(edits, 1);
}

#[tokio::test]
async fn reject_on_missing_unit_is_404() {
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state);
    let res = app
        .oneshot(
            Request::post(format!("/api/v1/units/{}/reject", Uuid::new_v4()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}
