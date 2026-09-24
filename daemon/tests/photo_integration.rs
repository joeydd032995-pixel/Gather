//! End-to-end test for the photo pipeline (Phase D) against pgvector:
//! hashing -> near-duplicate group with the sharpest copy as representative ->
//! album -> thumbnail, plus opt-in captions and visual topics against a mock
//! Ollama bound to loopback (nothing leaves the machine). Skipped without
//! DATABASE_URL. One test fn, so no two photo passes race within the binary.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use chrono::{Duration, TimeZone, Utc};
use http_body_util::BodyExt;
use image::codecs::jpeg::JpegEncoder;
use image::{imageops, RgbImage};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use uuid::Uuid;

use gather_daemon::config::Config;
use gather_daemon::photo::worker::{run_one_pass, vision_client};
use gather_daemon::{db, routes, AppState};

async fn test_state(config: Config) -> AppState {
    let pool = db::connect(&config.database_url).await.expect("db connect");
    db::migrate(&pool).await.expect("migrations");
    AppState {
        pool,
        config: Arc::new(config),
        metrics: metrics_exporter_prometheus::PrometheusBuilder::new()
            .build_recorder()
            .handle(),
        ollama: None,
        rate_limiter: None,
    }
}

fn scene() -> RgbImage {
    RgbImage::from_fn(480, 360, |x, y| {
        let v = 128.0 + 70.0 * (x as f32 / 37.0).sin() * (y as f32 / 23.0).cos() + x as f32 / 8.0;
        let l = v.clamp(0.0, 255.0) as u8;
        image::Rgb([l, l.wrapping_add(40), 255 - l])
    })
}

fn jpeg(img: &RgbImage, quality: u8) -> Vec<u8> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality)
        .encode_image(img)
        .unwrap();
    out
}

/// Store a photo the way ingestion does: an artifact holding the bytes plus
/// its images row (dimensions + capture time as the extractor would set).
async fn seed_photo(
    state: &AppState,
    bytes: &[u8],
    size: (i32, i32),
    taken_at: chrono::DateTime<Utc>,
) -> Uuid {
    // A per-run hash: the same synthetic bytes are re-seeded on every run.
    let hash = hex::encode(Sha256::digest(Uuid::new_v4().as_bytes()));
    let artifact: Uuid = sqlx::query_scalar(
        "INSERT INTO artifacts (kind, original_filename, media_type, byte_size, content_hash, raw_content) \
         VALUES ('image_photo', $1, 'image/jpeg', $2, $3, $4) RETURNING id",
    )
    .bind(format!("photo-{}.jpg", &hash[..8]))
    .bind(bytes.len() as i64)
    .bind(&hash)
    .bind(bytes)
    .fetch_one(&state.pool)
    .await
    .expect("seed artifact");
    sqlx::query_scalar(
        "INSERT INTO images (artifact_id, width, height, taken_at, ocr_status) \
         VALUES ($1, $2, $3, $4, 'completed') RETURNING id",
    )
    .bind(artifact)
    .bind(size.0)
    .bind(size.1)
    .bind(taken_at)
    .fetch_one(&state.pool)
    .await
    .expect("seed image")
}

/// A stand-in for a local Ollama: fixed caption, fixed 768-d embedding.
async fn spawn_mock_ollama() -> String {
    let app = Router::new()
        .route(
            "/api/generate",
            post(|| async { Json(json!({ "response": "A striped test pattern." })) }),
        )
        .route(
            "/api/embed",
            post(|Json(body): Json<Value>| async move {
                let n = body["input"].as_array().map_or(1, Vec::len);
                let embedding: Vec<f32> = (0..768).map(|i| (i % 7) as f32 / 7.0).collect();
                Json(json!({ "embeddings": vec![embedding; n] }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn column(state: &AppState, sql: &'static str, id: Uuid) -> Option<Uuid> {
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(&state.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn photos_are_hashed_grouped_albumed_thumbnailed_and_captioned() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping integration test: DATABASE_URL not set");
        return;
    };
    let mut config = Config::for_tests(database_url);
    config.ollama_url = Some(spawn_mock_ollama().await);
    config.ollama_vision_model = Some("mock-vision".to_string());
    config.photo_batch = 500;
    let state = test_state(config).await;
    let vision = vision_client(&state.config).expect("vision client");

    let original = scene();
    let half = imageops::resize(&original, 240, 180, imageops::FilterType::Triangle);
    let day = Utc.with_ymd_and_hms(2026, 5, 3, 9, 0, 0).unwrap();
    let sharp = seed_photo(&state, &jpeg(&original, 90), (480, 360), day).await;
    let small = seed_photo(
        &state,
        &jpeg(&half, 70),
        (240, 180),
        day + Duration::minutes(5),
    )
    .await;
    let later = seed_photo(
        &state,
        &jpeg(&original, 80),
        (480, 360),
        day + Duration::days(40),
    )
    .await;

    // Other suites may have left unprepared images; pass until ours are done.
    for _ in 0..20 {
        run_one_pass(&state.pool, &state.config, Some(&vision))
            .await
            .expect("photo pass");
        let pending: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM images WHERE id = ANY($1) \
             AND (photo_grouped_at IS NULL OR captioned_at IS NULL)",
        )
        .bind(vec![sharp, small, later])
        .fetch_one(&state.pool)
        .await
        .unwrap();
        if pending == 0 {
            break;
        }
    }

    // Near-duplicates: all three copies share one group; the sharpest,
    // earliest copy represents it.
    let dup = column(
        &state,
        "SELECT dup_cluster_id FROM images WHERE id = $1",
        sharp,
    )
    .await
    .expect("grouped as duplicates");
    for id in [small, later] {
        assert_eq!(
            column(
                &state,
                "SELECT dup_cluster_id FROM images WHERE id = $1",
                id
            )
            .await,
            Some(dup)
        );
    }
    let (kind, representative): (String, Option<Uuid>) =
        sqlx::query_as("SELECT kind, representative_id FROM clusters WHERE id = $1")
            .bind(dup)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(kind, "photo_dup");
    assert_eq!(representative, Some(sharp));

    // Albums: the two same-morning shots form one; the lone shot 40 days
    // later is below the minimum album size.
    let album = column(
        &state,
        "SELECT album_cluster_id FROM images WHERE id = $1",
        sharp,
    )
    .await
    .expect("album assigned");
    assert_eq!(
        column(
            &state,
            "SELECT album_cluster_id FROM images WHERE id = $1",
            small
        )
        .await,
        Some(album)
    );
    assert_eq!(
        column(
            &state,
            "SELECT album_cluster_id FROM images WHERE id = $1",
            later
        )
        .await,
        None
    );
    let label: String = sqlx::query_scalar("SELECT label FROM clusters WHERE id = $1")
        .bind(album)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(label, "2026-05-03");

    // Captions + visual topic from the (mock) local vision model.
    let caption: Option<String> = sqlx::query_scalar("SELECT caption FROM images WHERE id = $1")
        .bind(sharp)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(caption.as_deref(), Some("A striped test pattern."));
    // Every photo after the first captioned one finds an identical neighbour,
    // so at most one of ours (if it was captioned first overall) is untagged.
    let mut tagged = 0;
    for id in [sharp, small, later] {
        if column(
            &state,
            "SELECT topic_cluster_id FROM images WHERE id = $1",
            id,
        )
        .await
        .is_some()
        {
            tagged += 1;
        }
    }
    assert!(
        tagged >= 2,
        "identical captions should share a visual topic"
    );

    // Thumbnails: a small JPEG for a stored photo, 404 for an unknown id.
    let app = routes::build_router(state.clone());
    let res = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/images/{sharp}/thumbnail"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["content-type"], "image/jpeg");
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let thumb = image::load_from_memory(&bytes).expect("valid jpeg");
    assert!(thumb.width().max(thumb.height()) <= 256);

    let res = app
        .oneshot(
            Request::get(format!("/api/v1/images/{}/thumbnail", Uuid::new_v4()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}
