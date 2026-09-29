//! Automatic import against a real Postgres (pgvector): the inbox folder and
//! Claude Code's session folder. Skipped without DATABASE_URL, like the other
//! integration tests. Each test works in its own temporary folder.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

use gather_daemon::autoimport;
use gather_daemon::config::Config;
use gather_daemon::{db, AppState};

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

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gather-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write a file and make it look untouched for a minute, as a finished
/// download or a paused session does.
fn put(path: &Path, bytes: &[u8], age_secs: u64) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
    age(path, age_secs);
}

fn age(path: &Path, secs: u64) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(secs))
        .unwrap();
}

fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write;
    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buffer);
        for (name, bytes) in files {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }
    buffer.into_inner()
}

fn claude_export(marker: &str, said: &str) -> Vec<u8> {
    json!([{
        "uuid": format!("conv-{marker}"),
        "name": format!("chat {marker}"),
        "created_at": "2026-03-01T09:00:00Z",
        "updated_at": "2026-03-01T09:01:00Z",
        "chat_messages": [
            {"uuid": format!("m1-{marker}"), "sender": "human", "text": said,
             "created_at": "2026-03-01T09:00:00Z"},
            {"uuid": format!("m2-{marker}"), "sender": "assistant", "text": "Noted.",
             "created_at": "2026-03-01T09:00:05Z"}
        ]
    }])
    .to_string()
    .into_bytes()
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[tokio::test]
async fn the_inbox_reads_exports_and_documents_and_sorts_the_rest() {
    let Some(state) = test_state().await else {
        return;
    };
    let marker = Uuid::new_v4().simple().to_string();
    let inbox = temp_dir("inbox");

    let json_name = format!("claude-{marker}.json");
    let md_name = format!("perplexity-{marker}.md");
    let doc_name = format!("notes-{marker}.md");
    let zip_name = format!("export-{marker}.zip");
    let unknown_name = format!("mystery-{marker}.json");
    let broken_name = format!("broken-{marker}.json");
    let unsettled_name = format!("still-copying-{marker}.json");

    put(
        &inbox.join(&json_name),
        &claude_export(&format!("a{marker}"), &format!("I use Postgres {marker}.")),
        60,
    );
    put(
        &inbox.join(&md_name),
        format!(
            "<img src=\"https://r2cdn.perplexity.ai/x.png\"/>\n\n# Which database {marker}?\n\n\
             Postgres.[^1_1]\n\n<div align=\"center\">⁂</div>\n\n[^1_1]: https://example.com/pg\n"
        )
        .as_bytes(),
        60,
    );
    put(
        &inbox.join(&doc_name),
        format!("# Notes {marker}\n\nI keep my notes in plain files.\n").as_bytes(),
        60,
    );
    put(
        &inbox.join(&zip_name),
        &zip_of(&[
            (
                "conversations.json",
                &claude_export(&format!("z{marker}"), &format!("Zip chat {marker}.")),
            ),
            ("user.json", br#"{"id": 1}"#),
            ("chat.html", b"<html></html>"),
        ]),
        60,
    );
    put(&inbox.join(&unknown_name), br#"{"hello": "world"}"#, 60);
    put(&inbox.join(&broken_name), b"this is not json", 60);
    // Written a moment ago: left alone until it has stopped changing.
    put(&inbox.join(&unsettled_name), b"[]", 0);

    let more = autoimport::scan_inbox(&state, &inbox).await.unwrap();
    assert!(!more);

    assert_eq!(
        names_in(&inbox.join("done")),
        {
            let mut done = vec![
                json_name.clone(),
                md_name.clone(),
                doc_name.clone(),
                zip_name.clone(),
            ];
            done.sort();
            done
        },
        "everything read is moved to done/"
    );
    let failed = names_in(&inbox.join("failed"));
    assert!(failed.contains(&unknown_name) && failed.contains(&broken_name));
    assert!(
        failed.contains(&format!("{unknown_name}.why.txt"))
            && failed.contains(&format!("{broken_name}.why.txt")),
        "each file that could not be read says why: {failed:?}"
    );
    let why = std::fs::read_to_string(inbox.join("failed").join(format!("{unknown_name}.why.txt")))
        .unwrap();
    assert!(why.contains("not an export Gather knows"), "{why}");
    assert!(
        inbox.join(&unsettled_name).exists(),
        "a file still being written is not touched"
    );

    // What arrived: a Claude export, a Perplexity thread, a document, and the
    // chat inside the zip.
    let conversations = sqlx::query(
        "SELECT c.source_platform, count(m.id) AS messages \
         FROM conversations c JOIN messages m ON m.conversation_id = c.id \
         JOIN artifacts a ON a.id = c.artifact_id \
         WHERE a.original_filename = ANY($1) GROUP BY c.id, c.source_platform",
    )
    .bind(vec![
        json_name.clone(),
        md_name.clone(),
        "conversations.json".to_string(),
    ])
    .fetch_all(&state.pool)
    .await
    .unwrap();
    let platforms: Vec<String> = conversations
        .iter()
        .map(|r| r.get("source_platform"))
        .collect();
    assert!(platforms.contains(&"claude".to_string()));
    assert!(platforms.contains(&"perplexity".to_string()));
    let (documents,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM artifacts WHERE original_filename = $1 AND kind = 'document_markdown'",
    )
    .bind(&doc_name)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(documents, 1);
    let (zip_chat,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM messages WHERE content LIKE '%' || $1 || '%'")
            .bind(format!("Zip chat {marker}"))
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(zip_chat, 1, "the export inside the zip was read");

    // The record of what was done is kept.
    let (recorded,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM import_sources WHERE kind = 'inbox' AND path LIKE '%' || $1 || '%'",
    )
    .bind(&marker)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(recorded, 6);

    // A second look finds nothing new to do.
    assert!(!autoimport::scan_inbox(&state, &inbox).await.unwrap());
    assert_eq!(names_in(&inbox.join("done")).len(), 4);

    std::fs::remove_dir_all(&inbox).unwrap();
}

fn session_line(kind: &str, uuid: &str, parent: Option<&str>, text: &str, at: &str) -> String {
    json!({
        "type": kind, "uuid": uuid, "parentUuid": parent, "sessionId": "SESSION",
        "timestamp": at, "cwd": "/work/app",
        "message": {"role": kind, "content": [{"type": "text", "text": text}]},
    })
    .to_string()
}

#[tokio::test]
async fn claude_code_sessions_are_imported_and_grow_without_repeating() {
    let Some(state) = test_state().await else {
        return;
    };
    let marker = Uuid::new_v4().simple().to_string();
    let session_id = format!("session-{marker}");
    let root = temp_dir("sessions");
    let file = root.join("-work-app").join(format!("{session_id}.jsonl"));
    let line = |kind: &str, uuid: &str, parent: Option<&str>, text: &str, at: &str| {
        session_line(kind, uuid, parent, text, at).replace("SESSION", &session_id)
    };

    // Two messages, and a tool result that must not be kept.
    let mut text = String::new();
    text += &line(
        "user",
        "u1",
        None,
        &format!("I deploy with Docker {marker}."),
        "2026-03-01T09:00:00Z",
    );
    text.push('\n');
    text += &line(
        "assistant",
        "a1",
        Some("u1"),
        "Good choice.",
        "2026-03-01T09:00:05Z",
    );
    text.push('\n');
    text += &json!({
        "type": "user", "uuid": "t1", "parentUuid": "a1", "sessionId": session_id,
        "message": {"role": "user", "content": [{"type": "tool_result", "content": format!("SECRET {marker}")}]},
    })
    .to_string();
    text.push('\n');
    put(&file, text.as_bytes(), 60);
    // A sub-agent log with no conversation in it.
    let quiet = root.join("-work-app").join(format!("agent-{marker}.jsonl"));
    put(
        &quiet,
        json!({"type": "user", "isSidechain": true, "sessionId": "other",
               "message": {"role": "user", "content": "chatter"}})
        .to_string()
        .as_bytes(),
        60,
    );
    // Still being written: not read yet.
    let active = root
        .join("-work-app")
        .join(format!("active-{marker}.jsonl"));
    put(&active, text.as_bytes(), 0);

    assert!(!autoimport::scan_claude_code(&state, &root).await.unwrap());

    let conversation = sqlx::query(
        "SELECT c.id, c.title, c.artifact_id FROM conversations c \
         WHERE c.source_platform = 'claude_code' AND c.external_id = $1",
    )
    .bind(&session_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let conversation_id: Uuid = conversation.get("id");
    let artifact_id: Uuid = conversation.get("artifact_id");
    assert!(conversation
        .get::<Option<String>, _>("title")
        .unwrap()
        .starts_with("I deploy with Docker"));
    let rows = sqlx::query(
        "SELECT role, content, seq FROM messages WHERE conversation_id = $1 ORDER BY seq",
    )
    .bind(conversation_id)
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "the tool result is not a message");
    assert_eq!(rows[0].get::<String, _>("role"), "user");
    assert_eq!(rows[1].get::<String, _>("content"), "Good choice.");
    let (leaked,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM messages WHERE content LIKE '%' || $1 || '%'")
            .bind(format!("SECRET {marker}"))
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(leaked, 0);
    let (sidechain,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM conversations WHERE source_platform = 'claude_code' AND external_id = 'other'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(sidechain, 0, "a sub-agent log adds no conversation");

    // An unchanged file is not read again.
    autoimport::scan_claude_code(&state, &root).await.unwrap();
    let (count,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM messages WHERE conversation_id = $1")
            .bind(conversation_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(count, 2);

    // The session grows: only the new messages are added, to the same
    // conversation, and the stored transcript follows.
    let mut grown = text.clone();
    grown += &line(
        "user",
        "u2",
        Some("a1"),
        &format!("Now add a health check {marker}."),
        "2026-03-01T09:10:00Z",
    );
    grown.push('\n');
    std::fs::write(&file, grown.as_bytes()).unwrap();
    age(&file, 30);
    autoimport::scan_claude_code(&state, &root).await.unwrap();

    let rows =
        sqlx::query("SELECT content, seq FROM messages WHERE conversation_id = $1 ORDER BY seq")
            .bind(conversation_id)
            .fetch_all(&state.pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 3);
    let seqs: Vec<i32> = rows.iter().map(|r| r.get("seq")).collect();
    assert_eq!(seqs, [0, 1, 2]);
    assert!(rows[2].get::<String, _>("content").contains("health check"));
    let (conversations,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM conversations WHERE source_platform = 'claude_code' AND external_id = $1",
    )
    .bind(&session_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(conversations, 1);
    let row = sqlx::query("SELECT raw_content, byte_size FROM artifacts WHERE id = $1")
        .bind(artifact_id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    let raw: Vec<u8> = row.get("raw_content");
    assert!(String::from_utf8(raw).unwrap().contains("health check"));

    // Nothing new: nothing added.
    autoimport::scan_claude_code(&state, &root).await.unwrap();
    let (count,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM messages WHERE conversation_id = $1")
            .bind(conversation_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(count, 3);

    // The active session was left for later.
    let (active_seen,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM import_sources WHERE path LIKE '%' || $1 || '%'")
            .bind(format!("active-{marker}"))
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(active_seen, 0);

    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn two_sessions_that_say_the_same_things_are_both_kept() {
    let Some(state) = test_state().await else {
        return;
    };
    let marker = Uuid::new_v4().simple().to_string();
    let root = temp_dir("twins");
    // A session and a copy of it under another id: same messages, same
    // message ids, same times.
    let body = |id: &str| {
        let mut text = String::new();
        for (uuid, kind, said, at) in [
            (
                "u1",
                "user",
                format!("I back up with restic {marker}."),
                "2026-03-01T09:00:00Z",
            ),
            (
                "a1",
                "assistant",
                "Good.".to_string(),
                "2026-03-01T09:00:05Z",
            ),
        ] {
            text += &session_line(kind, uuid, None, &said, at).replace("SESSION", id);
            text.push('\n');
        }
        text
    };
    let first = format!("first-{marker}");
    let second = format!("second-{marker}");
    put(
        &root.join("p").join("first.jsonl"),
        body(&first).as_bytes(),
        60,
    );
    put(
        &root.join("p").join("second.jsonl"),
        body(&second).as_bytes(),
        60,
    );

    autoimport::scan_claude_code(&state, &root).await.unwrap();

    for id in [&first, &second] {
        let (messages,): (i64,) = sqlx::query_as(
            "SELECT count(m.id) FROM conversations c JOIN messages m ON m.conversation_id = c.id \
             WHERE c.source_platform = 'claude_code' AND c.external_id = $1",
        )
        .bind(id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
        assert_eq!(messages, 2, "session {id} keeps its own conversation");
    }
    let (sessions,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM import_sources WHERE kind = 'claude_code' AND status = 'imported' \
         AND session_id LIKE '%' || $1",
    )
    .bind(&marker)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(sessions, 2);

    std::fs::remove_dir_all(&root).unwrap();
}
