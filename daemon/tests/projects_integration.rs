//! Projects end to end against pgvector: a folder uploaded file by file with
//! its paths, a .zip unpacked into a project (and zips inside it), the tree
//! they produce, the new document formats, files kept as they are, and what
//! is left out or skipped (and why). Skipped without
//! DATABASE_URL. Tests share one database, so every test holds one lock.

use std::io::{Cursor, Write};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

use gather_daemon::config::Config;
use gather_daemon::{db, extract, routes, AppState};

static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
        ollama: None,
        rate_limiter: None,
    })
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn call(
    app: &axum::Router,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(format!("/api/v1{path}"));
    let body = match body {
        Some(b) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    send(app, req.body(body).unwrap()).await
}

enum Part<'a> {
    Text(&'a str, &'a str),
    File(&'a str, &'a [u8]),
}

async fn multipart(app: &axum::Router, path: &str, parts: &[Part<'_>]) -> (StatusCode, Value) {
    let boundary = "gatherprojectboundary";
    let mut body = Vec::new();
    for part in parts {
        match part {
            Part::Text(name, value) => write!(
                body,
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .unwrap(),
            Part::File(filename, bytes) => {
                write!(
                    body,
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
                )
                .unwrap();
                body.extend_from_slice(bytes);
                body.extend_from_slice(b"\r\n");
            }
        }
    }
    write!(body, "--{boundary}--\r\n").unwrap();
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/v1{path}"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap();
    send(app, req).await
}

fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, data) in entries {
        if name.ends_with('/') {
            w.add_directory(*name, opts).unwrap();
        } else {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
    }
    w.finish().unwrap().into_inner()
}

fn docx(paragraphs: &[&str]) -> Vec<u8> {
    let body: String = paragraphs
        .iter()
        .map(|p| format!("<w:p><w:r><w:t>{p}</w:t></w:r></w:p>"))
        .collect();
    let xml = format!(
        r#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}</w:body></w:document>"#
    );
    zip_of(&[("word/document.xml", xml.as_bytes())])
}

fn item<'a>(detail: &'a Value, path: &str) -> &'a Value {
    detail["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["path"] == path)
        .unwrap_or_else(|| panic!("no item {path} in {detail}"))
}

#[tokio::test]
async fn a_folder_uploaded_file_by_file_keeps_its_tree() {
    let _guard = LOCK.lock().await;
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());
    let t = &Uuid::new_v4().simple().to_string()[..8];
    let (s, project) = call(
        &app,
        Method::POST,
        "/projects",
        Some(json!({"name": format!("Atlas {t}")})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{project}");
    let id = project["id"].as_str().unwrap().to_string();

    let plan =
        format!("# Plan {t}\n\nDana Reyes owns the Atlas{t} budget. The budget is $40,000.\n");
    let report = docx(&[
        &format!("Atlas{t} status report"),
        "Launch is planned for May.",
    ]);
    let page = format!("<html><body><h1>Atlas{t}</h1><p>Hosted on Fly.</p></body></html>");
    // Every file counts: a program is kept as it is, text with a name Gather
    // doesn't know is read as text, a damaged Word file is still kept, and a
    // .zip is unpacked where it sits (the one inside it too).
    let mut tool = b"MZ\x00\x01binary".to_vec();
    tool.extend_from_slice(t.as_bytes());
    let todo = format!("- ship Atlas{t}\n");
    let broken = format!("PK not really a Word file {t}");
    let a_md = format!("# Bundle {t}\n");
    let c_md = format!("# Deeper {t}\n");
    let deeper = zip_of(&[("c.md", c_md.as_bytes())]);
    let bundle = zip_of(&[
        ("bundle/", b""),
        ("bundle/a.md", a_md.as_bytes()),
        ("bundle/deeper.zip", &deeper),
    ]);
    let (s, body) = multipart(
        &app,
        &format!("/projects/{id}/files"),
        &[
            Part::Text("path", "docs/plan.md"),
            Part::File("plan.md", plan.as_bytes()),
            Part::Text("path", "docs/reports/status.docx"),
            Part::File("status.docx", &report),
            Part::Text("path", "src/app.ts"),
            Part::File(
                "app.ts",
                format!("export const name = 'atlas{t}';\n").as_bytes(),
            ),
            Part::Text("path", "data/people.csv"),
            Part::File(
                "people.csv",
                format!("name,role\nDana{t},owner\n").as_bytes(),
            ),
            Part::Text("path", "site/index.html"),
            Part::File("index.html", page.as_bytes()),
            Part::Text("path", ".env"),
            Part::File(".env", b"API_KEY=hunter2"),
            Part::Text("path", ".git/config"),
            Part::File("config", b"[core]"),
            Part::Text("path", "bin/tool.exe"),
            Part::File("tool.exe", &tool),
            Part::Text("path", "notes/todo.xyz"),
            Part::File("todo.xyz", todo.as_bytes()),
            Part::Text("path", "docs/broken.docx"),
            Part::File("broken.docx", broken.as_bytes()),
            Part::Text("path", "archive/bundle.zip"),
            Part::File("bundle.zip", &bundle),
            Part::Text("left_out", "web/node_modules"),
            Part::Text("left_out", "docs2"),
            Part::Text("path", "../escape.md"),
            Part::File("escape.md", b"# nope"),
        ],
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{body}");
    let by_path = |p: &str| {
        body["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"] == p)
            .cloned()
            .unwrap_or_else(|| panic!("no result for {p}: {body}"))
    };
    assert_eq!(by_path("docs/plan.md")["status"], "ingested");
    assert_eq!(by_path("docs/reports/status.docx")["kind"], "document_docx");
    assert_eq!(by_path("src/app.ts")["kind"], "document_text");
    assert_eq!(by_path("site/index.html")["status"], "ingested");
    assert_eq!(by_path(".env")["status"], "skipped");
    assert_eq!(by_path(".git")["status"], "left_out");
    assert_eq!(by_path("bin/tool.exe")["status"], "stored");
    assert_eq!(by_path("bin/tool.exe")["kind"], "file_other");
    assert_eq!(by_path("notes/todo.xyz")["status"], "ingested");
    assert_eq!(by_path("notes/todo.xyz")["kind"], "document_text");
    assert_eq!(by_path("docs/broken.docx")["status"], "stored");
    assert!(by_path("docs/broken.docx")["detail"]
        .as_str()
        .unwrap()
        .contains("its text couldn't be read"));
    assert_eq!(by_path("archive/bundle.zip/a.md")["status"], "ingested");
    assert_eq!(
        by_path("archive/bundle.zip/deeper.zip/c.md")["status"],
        "ingested"
    );
    assert_eq!(by_path("web/node_modules")["status"], "left_out");
    assert_eq!(by_path("docs2")["status"], "skipped");
    assert_eq!(by_path("../escape.md")["status"], "skipped");
    assert!(body["stopped"].is_null());

    // The HTML was read as text, not markup.
    let html_artifact = by_path("site/index.html")["artifact_id"]
        .as_str()
        .unwrap()
        .to_string();
    let text: String =
        sqlx::query_scalar("SELECT extracted_text FROM documents WHERE artifact_id = $1::uuid")
            .bind(&html_artifact)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert!(
        text.contains("Hosted on Fly.") && !text.contains("<p>"),
        "{text}"
    );

    let (s, detail) = call(&app, Method::GET, &format!("/projects/{id}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(item(&detail, "docs")["item_kind"], "folder");
    let reports = item(&detail, "docs/reports");
    assert_eq!(reports["parent_id"], item(&detail, "docs")["id"]);
    assert_eq!(
        item(&detail, "docs/reports/status.docx")["parent_id"],
        reports["id"]
    );
    assert_eq!(item(&detail, "docs/reports/status.docx")["depth"], 2);
    assert_eq!(item(&detail, ".env")["status"], "skipped");
    assert!(item(&detail, ".env")["detail"]
        .as_str()
        .unwrap()
        .contains("passwords"));
    assert_eq!(item(&detail, "bin/tool.exe")["status"], "stored");
    assert_eq!(item(&detail, "bin/tool.exe")["artifact_kind"], "file_other");
    assert_eq!(item(&detail, "archive/bundle.zip")["item_kind"], "folder");
    assert_eq!(
        item(&detail, "archive/bundle.zip/deeper.zip")["item_kind"],
        "folder"
    );
    // Left-out folders are in the tree, with why, and nothing inside them.
    let git = item(&detail, ".git");
    assert_eq!(git["item_kind"], "folder");
    assert_eq!(git["status"], "skipped");
    assert!(git["detail"].as_str().unwrap().contains("version-control"));
    assert_eq!(item(&detail, "web/node_modules")["status"], "skipped");
    assert!(!detail["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["path"].as_str().unwrap().starts_with(".git/")));
    assert!(!detail["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["path"] == "docs2"));
    assert!(!detail["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["path"].as_str().unwrap().contains("escape")));
    assert_eq!(detail["files"], 11);
    assert_eq!(detail["ingested"], 8);
    assert_eq!(detail["stored"], 2);
    assert_eq!(detail["skipped"], 1);
    assert_eq!(detail["left_out"], 2);
    // docs, docs/reports, src, data, site, bin, notes, archive, the two
    // unpacked zips, and web (holding node_modules); the left-out folders
    // themselves aren't counted as folders.
    assert_eq!(detail["folders"], 11);

    // Extraction runs over project files like any other.
    for _ in 0..20 {
        extract::run_one_pass(&state.pool, &state.config, None)
            .await
            .unwrap();
    }
    let (_, detail) = call(&app, Method::GET, &format!("/projects/{id}"), None).await;
    assert!(
        item(&detail, "docs/plan.md")["units"].as_i64().unwrap() > 0,
        "{detail}"
    );

    // The same content at another path is the same file in Gather.
    let (_, body) = multipart(
        &app,
        &format!("/projects/{id}/files"),
        &[
            Part::Text("path", "copy/plan.md"),
            Part::File("plan.md", plan.as_bytes()),
        ],
    )
    .await;
    assert_eq!(body["files"][0]["status"], "deduplicated");
    assert_eq!(
        body["files"][0]["artifact_id"],
        by_path("docs/plan.md")["artifact_id"]
    );

    // A file can't take a folder's place, nor sit inside a file. Each is
    // skipped with the reason, without being stored, and the rest of the
    // upload goes on.
    let later = format!("Later notes {t}.\n");
    let (s, body) = multipart(
        &app,
        &format!("/projects/{id}/files"),
        &[
            Part::Text("path", "docs"),
            Part::File("docs", b"clash one"),
            Part::Text("path", "docs/plan.md/inside.txt"),
            Part::File("inside.txt", b"clash two"),
            Part::Text("path", "later.md"),
            Part::File("later.md", later.as_bytes()),
        ],
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{body}");
    let files = body["files"].as_array().unwrap();
    assert_eq!(files[0]["status"], "skipped", "{body}");
    assert!(files[0]["detail"]
        .as_str()
        .unwrap()
        .contains("is a folder in this project"));
    assert!(files[0]["artifact_id"].is_null());
    assert_eq!(files[1]["status"], "skipped", "{body}");
    assert!(files[1]["detail"]
        .as_str()
        .unwrap()
        .contains("'docs/plan.md' is a file in this project"));
    assert!(files[1]["artifact_id"].is_null());
    assert_eq!(files[2]["status"], "ingested", "{body}");
    let (_, detail) = call(&app, Method::GET, &format!("/projects/{id}"), None).await;
    assert_eq!(item(&detail, "docs")["item_kind"], "folder");
    assert_eq!(item(&detail, "docs/plan.md")["item_kind"], "file");

    // What a folder held but the sender didn't send: a secret-looking file
    // withheld unread is listed as skipped; any other path must be sent;
    // an empty folder keeps its place in the tree.
    let (s, body) = multipart(
        &app,
        &format!("/projects/{id}/files"),
        &[
            Part::Text("withheld", ".ssh/id_rsa"),
            Part::Text("withheld", "notes/plain.md"),
            Part::Text("folder", "drafts/empty"),
        ],
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{body}");
    let files = body["files"].as_array().unwrap();
    assert_eq!(files.len(), 2, "{body}");
    assert_eq!(files[0]["status"], "skipped");
    assert!(files[0]["detail"].as_str().unwrap().contains("passwords"));
    assert_eq!(files[1]["status"], "skipped");
    assert!(files[1]["detail"]
        .as_str()
        .unwrap()
        .contains("send this one"));
    let (_, detail) = call(&app, Method::GET, &format!("/projects/{id}"), None).await;
    assert_eq!(item(&detail, ".ssh/id_rsa")["status"], "skipped");
    assert!(item(&detail, ".ssh/id_rsa")["artifact_id"].is_null());
    assert_eq!(item(&detail, "drafts/empty")["item_kind"], "folder");
    assert!(!detail["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["path"] == "notes/plain.md"));

    // A request with nothing in it is refused, and its job is finished
    // rather than left running.
    let (s, _) = multipart(
        &app,
        &format!("/projects/{id}/files"),
        &[Part::Text("path", "dangling.md")],
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let status: String = sqlx::query_scalar(
        "SELECT status::text FROM ingestion_jobs WHERE stats->>'project' = $1 \
         ORDER BY started_at DESC LIMIT 1",
    )
    .bind(&id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert!(
        !["pending", "processing"].contains(&status.as_str()),
        "{status}"
    );

    // Removing the project keeps its files in Gather.
    let (s, _) = call(&app, Method::DELETE, &format!("/projects/{id}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = call(&app, Method::GET, &format!("/projects/{id}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let still: i64 = sqlx::query_scalar("SELECT count(*) FROM artifacts WHERE id = $1::uuid")
        .bind(&html_artifact)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(still, 1);
}

#[tokio::test]
async fn a_zip_becomes_a_project_named_after_its_folder() {
    let _guard = LOCK.lock().await;
    let Some(state) = test_state().await else {
        return;
    };
    let app = routes::build_router(state.clone());
    let t = &Uuid::new_v4().simple().to_string()[..8];
    let root = format!("Borealis{t}");
    let readme = format!("# {root}\n\nBorealis{t} ships in June.\n");
    let spec = format!("Spec for {root}.");
    let zeros = vec![b'a'; 3 * 1024 * 1024];
    let names = [
        format!("{root}/"),
        format!("{root}/README.md"),
        format!("{root}/specs/api.txt"),
        format!("{root}/empty/"),
        format!("{root}/node_modules/x/index.js"),
        format!("{root}/nested/old.zip"),
        format!("{root}/bomb.txt"),
        format!("{root}/deep/z2.zip"),
    ];
    // Three archives inside this one, one inside the other: the first two
    // are unpacked, the third is kept as it is.
    let z4 = zip_of(&[("c.md", format!("# Deepest {t}\n").as_bytes())]);
    let z3 = zip_of(&[("z4.zip", &z4)]);
    let z2 = zip_of(&[("z3.zip", &z3)]);
    let not_a_zip = format!("PK not really a zip {t}");
    let zip = zip_of(&[
        (&names[0], b""),
        (&names[1], readme.as_bytes()),
        (&names[2], spec.as_bytes()),
        (&names[3], b""),
        (&names[4], b"module.exports = 1"),
        (&names[5], not_a_zip.as_bytes()),
        (&names[6], &zeros),
        (&names[7], &z2),
    ]);
    let (s, report) = multipart(
        &app,
        "/projects/import",
        &[Part::File("borealis.zip", &zip)],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{report}");
    assert_eq!(report["project"]["name"], root.as_str());
    assert_eq!(report["project"]["source"], "zip");
    let id = report["project"]["id"].as_str().unwrap();

    let (_, detail) = call(&app, Method::GET, &format!("/projects/{id}"), None).await;
    assert_eq!(item(&detail, "README.md")["status"], "ingested");
    assert_eq!(item(&detail, "specs")["item_kind"], "folder");
    assert_eq!(item(&detail, "specs/api.txt")["status"], "ingested");
    assert_eq!(item(&detail, "empty")["item_kind"], "folder");
    // Not really a zip: kept as it is, with why.
    assert_eq!(item(&detail, "nested/old.zip")["status"], "stored");
    assert!(item(&detail, "nested/old.zip")["detail"]
        .as_str()
        .unwrap()
        .contains("couldn't be unpacked"));
    assert_eq!(item(&detail, "deep/z2.zip/z3.zip")["item_kind"], "folder");
    let z4_item = item(&detail, "deep/z2.zip/z3.zip/z4.zip");
    assert_eq!(z4_item["status"], "stored");
    assert!(z4_item["detail"]
        .as_str()
        .unwrap()
        .contains("inside 3 others"));
    assert!(item(&detail, "bomb.txt")["detail"]
        .as_str()
        .unwrap()
        .contains("expands"));
    assert_eq!(item(&detail, "node_modules")["status"], "skipped");
    assert!(!detail["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["path"].as_str().unwrap().starts_with("node_modules/")));

    // A name given with the upload wins; a non-zip is refused.
    let (_, named) = multipart(
        &app,
        "/projects/import",
        &[Part::Text("name", "Renamed"), Part::File("x.zip", &zip)],
    )
    .await;
    assert_eq!(named["project"]["name"], "Renamed");
    // Same files again: already in Gather.
    assert!(named["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["status"] == "deduplicated"));
    let (s, _) = multipart(
        &app,
        "/projects/import",
        &[Part::File("x.zip", b"not a zip")],
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (_, list) = call(&app, Method::GET, "/projects", None).await;
    assert!(list["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"] == id));
}

#[tokio::test]
async fn the_projects_migration_is_reversible() {
    let _guard = LOCK.lock().await;
    let Some(state) = test_state().await else {
        return;
    };
    let name = format!("gather_proj_{}", &Uuid::new_v4().simple().to_string()[..12]);
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&state.pool)
        .await
        .unwrap();
    let url = state.config.database_url.clone();
    let base = url.rsplit_once('/').map(|(b, _)| b.to_string()).unwrap();
    let query = url
        .split_once('?')
        .map(|(_, q)| format!("?{q}"))
        .unwrap_or_default();
    let pool = db::connect(&format!("{base}/{name}{query}")).await.unwrap();
    db::migrate(&pool).await.unwrap();
    let exists = |pool: sqlx::PgPool| async move {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'project_items')",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    assert!(exists(pool.clone()).await);
    // A Word file still in Gather: the reverse refuses, rather than delete it
    // and leave what was learned from it without a source.
    let docx: Uuid = sqlx::query_scalar(
        "INSERT INTO artifacts (kind, byte_size, content_hash, raw_content) \
         VALUES ('document_docx', 1, repeat('a', 64), '\\x00') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    // One connection: the refused script leaves its transaction aborted
    // until rolled back.
    let mut conn = pool.acquire().await.unwrap();
    let refused = sqlx::raw_sql(include_str!("../migrations-down/0016_projects.down.sql"))
        .execute(&mut *conn)
        .await
        .unwrap_err();
    sqlx::raw_sql("ROLLBACK").execute(&mut *conn).await.unwrap();
    drop(conn);
    assert!(
        refused.to_string().contains("delete them through Gather"),
        "{refused}"
    );
    assert!(exists(pool.clone()).await, "nothing changed");
    sqlx::query("DELETE FROM artifacts WHERE id = $1")
        .bind(docx)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations-down/0016_projects.down.sql"))
        .execute(&pool)
        .await
        .unwrap();
    assert!(!exists(pool.clone()).await, "down removes it");
    let kinds: Vec<String> =
        sqlx::query_scalar("SELECT unnest(enum_range(NULL::artifact_kind))::text")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(!kinds.contains(&"document_docx".to_string()));
    sqlx::raw_sql(include_str!("../migrations/0016_projects.sql"))
        .execute(&pool)
        .await
        .unwrap();
    assert!(exists(pool.clone()).await, "and up applies again");
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE {name} WITH (FORCE)"
    )))
    .execute(&state.pool)
    .await
    .unwrap();
}
