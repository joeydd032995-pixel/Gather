//! End-to-end test for active learning + auto-tuning (Phase C) against
//! pgvector: feedback -> tuned threshold -> audit -> tray drain -> re-rank,
//! plus the tray accept/reject actions and reset. Skipped without DATABASE_URL.
//!
//! The tuner reads feedback globally, so this binary clears feedback and
//! tuning state first and resets tuning at the end (cargo runs test binaries
//! one at a time, so no other suite is mid-run). Everything is asserted in ONE
//! test fn so no two tuner passes race within the binary.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

use gather_daemon::config::Config;
use gather_daemon::decide::live::KEY_ADMIT_HOLD_BELOW;
use gather_daemon::routes::feedback::confirm_unit_core;
use gather_daemon::tune::worker::run_one_pass;
use gather_daemon::tune::Direction;
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

/// Clear feedback, tray and tuned values (and, first time, the tuning audit).
async fn clear_learned_state(state: &AppState, with_audit: bool) {
    let mut statements = vec![
        "DELETE FROM unit_feedback",
        "DELETE FROM decision_tuning",
        "DELETE FROM review_queue",
    ];
    if with_audit {
        statements.push("DELETE FROM decision_tuning_audit");
    }
    for sql in statements {
        sqlx::query(sql).execute(&state.pool).await.unwrap();
    }
}

async fn seed_unit(state: &AppState, confidence: f32) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO atomic_units (kind, statement, statement_hash, confidence, extraction_method) \
         VALUES ('fact', $1, $2, $3, 'rule_based') RETURNING id",
    )
    .bind(format!("tune fixture {}", Uuid::new_v4()))
    .bind(format!("tune-{}", Uuid::new_v4()))
    .bind(confidence)
    .fetch_one(&state.pool)
    .await
    .expect("seed unit")
}

async fn park_unit(state: &AppState, unit: Uuid, confidence: f32) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO review_queue (target_kind, target_id, reason, signals) \
         VALUES ('unit', $1, 'low-confidence', jsonb_build_object('confidence', $2::float4)) \
         RETURNING id",
    )
    .bind(unit)
    .bind(confidence)
    .fetch_one(&state.pool)
    .await
    .expect("park unit")
}

async fn seed_entity(state: &AppState, name: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO entities (name, kind) VALUES ($1, 'other') RETURNING id")
        .bind(name)
        .fetch_one(&state.pool)
        .await
        .expect("seed entity")
}

async fn review_state(state: &AppState, id: Uuid) -> String {
    sqlx::query_scalar("SELECT state FROM review_queue WHERE id = $1")
        .bind(id)
        .fetch_one(&state.pool)
        .await
        .unwrap()
}

async fn post(app: &axum::Router, uri: String, body: Value) -> (StatusCode, Value) {
    let res = app
        .clone()
        .oneshot(
            Request::post(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn feedback_tunes_thresholds_drains_the_tray_and_is_reversible() {
    let Some(state) = test_state().await else {
        return;
    };
    clear_learned_state(&state, true).await;
    let app = routes::build_router(state.clone());

    // --- Evidence that the hold bar (0.5) is too strict: every unit judged,
    // including a dozen just below the bar, was kept.
    for _ in 0..30 {
        let id = seed_unit(&state, 0.6).await;
        confirm_unit_core(&state.pool, id, None).await.unwrap();
    }
    for _ in 0..12 {
        let id = seed_unit(&state, 0.46).await;
        confirm_unit_core(&state.pool, id, None).await.unwrap();
    }
    // Two unlabelled held units: 0.47 will clear the lowered bar, 0.40 won't.
    let cleared_unit = seed_unit(&state, 0.47).await;
    let cleared = park_unit(&state, cleared_unit, 0.47).await;
    let still_held_unit = seed_unit(&state, 0.40).await;
    let still_held = park_unit(&state, still_held_unit, 0.40).await;

    let stats = run_one_pass(&state.pool, &state.config).await.unwrap();
    let change = stats
        .changes
        .iter()
        .find(|c| c.key == KEY_ADMIT_HOLD_BELOW)
        .expect("admission threshold should move");
    assert_eq!(change.direction, Direction::Lower);
    assert!((change.to - 0.46).abs() < 1e-4, "lowered to {}", change.to);

    let stored: f64 = sqlx::query_scalar("SELECT value FROM decision_tuning WHERE key = $1")
        .bind(KEY_ADMIT_HOLD_BELOW)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert!((stored - 0.46).abs() < 1e-4);
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM decision_tuning_audit WHERE key = $1 AND actor = 'auto-tuner'",
    )
    .bind(KEY_ADMIT_HOLD_BELOW)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(audited, 1);
    // A lowering is audited with the evidence at the NEW threshold.
    let reason: Value = sqlx::query_scalar(
        "SELECT reason FROM decision_tuning_audit WHERE key = $1 AND actor = 'auto-tuner'",
    )
    .bind(KEY_ADMIT_HOLD_BELOW)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(reason["lowering"]["support"], json!(42));
    assert_eq!(reason["lowering"]["region_support"], json!(12));

    // The tray drained itself, and the survivor was re-ranked.
    assert_eq!(stats.drained, 1);
    assert_eq!(review_state(&state, cleared).await, "dismissed");
    assert_eq!(review_state(&state, still_held).await, "open");
    let gain: f32 = sqlx::query_scalar("SELECT info_gain FROM review_queue WHERE id = $1")
        .bind(still_held)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert!(gain > 0.0);

    // --- Tray actions: rejecting a held unit retracts it; accepting a held
    // merge pair performs the merge. Both leave labels.
    let (status, _) = post(
        &app,
        format!("/api/v1/review/{still_held}/reject"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let unit_status: String =
        sqlx::query_scalar("SELECT status::text FROM atomic_units WHERE id = $1")
            .bind(still_held_unit)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(unit_status, "retracted");

    let base = format!("Tuning Pair {}", Uuid::new_v4());
    let a = seed_entity(&state, &base).await;
    let b = seed_entity(&state, &format!("{base} Inc")).await;
    let pair: Uuid = sqlx::query_scalar(
        "INSERT INTO review_queue (target_kind, target_id, reason, signals) \
         VALUES ('entity', $1, 'merge-band', $2) RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(json!({ "a": a, "b": b, "score": 0.8, "method": "text" }))
    .fetch_one(&state.pool)
    .await
    .unwrap();
    // The tray carries both entity names, so a UI needs no per-entity fetch.
    let res = app
        .clone()
        .oneshot(Request::get("/api/v1/review").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let tray: Value = serde_json::from_slice(&bytes).unwrap();
    let held = tray["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == json!(pair))
        .expect("held pair listed");
    assert_eq!(held["a_name"], json!(base));
    assert_eq!(held["b_name"], json!(format!("{base} Inc")));

    let (status, body) = post(&app, format!("/api/v1/review/{pair}/accept"), json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["winner"], json!(b), "longer name survives");
    let merged_into: Option<Uuid> =
        sqlx::query_scalar("SELECT merged_into_entity_id FROM entities WHERE id = $1")
            .bind(a)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(merged_into, Some(b));
    let merge_label: (String, Option<f32>) = sqlx::query_as(
        "SELECT action, score FROM unit_feedback WHERE target_kind = 'merge' LIMIT 1",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(merge_label.0, "confirm");
    assert!((merge_label.1.unwrap() - 0.8).abs() < 1e-6);

    // --- Reset returns to the env default, audited.
    let (status, body) = post(&app, "/api/v1/tuning/reset".to_string(), json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["reset"], json!([KEY_ADMIT_HOLD_BELOW]));
    let res = app
        .clone()
        .oneshot(Request::get("/api/v1/tuning").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let tuning: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(tuning["thresholds"][0]["tuned"], json!(false));
    assert!((tuning["thresholds"][0]["value"].as_f64().unwrap() - 0.5).abs() < 1e-6);

    // The reset is durable: the same pre-reset labels don't re-create the
    // learned value on the next pass.
    let stats = run_one_pass(&state.pool, &state.config).await.unwrap();
    assert!(stats.changes.is_empty(), "{:?}", stats.changes);
    let tuned: i64 = sqlx::query_scalar("SELECT count(*) FROM decision_tuning")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(tuned, 0);

    // Undone agreement-gated merges tighten the agreement bar, and only it:
    // a gate's corrections never move the other gate's threshold.
    for _ in 0..25 {
        sqlx::query(
            "INSERT INTO unit_feedback (target_kind, target_id, action, corrected, score) \
             VALUES ('merge', $1, 'reject', '{\"gate\": \"agreement\"}', 0.82)",
        )
        .bind(Uuid::new_v4())
        .execute(&state.pool)
        .await
        .unwrap();
    }
    let stats = run_one_pass(&state.pool, &state.config).await.unwrap();
    let agree = stats
        .changes
        .iter()
        .find(|c| c.key == "merge.agree")
        .expect("agreement bar should move");
    assert_eq!(agree.direction, Direction::Raise);
    assert!((agree.to - 0.85).abs() < 1e-4, "raised to {}", agree.to);
    assert!(stats.changes.iter().all(|c| c.key != "merge.auto_single"));

    // Leave no learned state behind for later suites.
    clear_learned_state(&state, false).await;
}
