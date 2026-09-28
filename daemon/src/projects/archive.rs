//! Unpacking a project `.zip`, one entry at a time.
//!
//! The archive is read on a blocking thread and entries are handed over a
//! channel of one, so at most one decompressed file is in memory however
//! large the project. Nothing is written to disk. Limits hold against zip
//! bombs: the file count, each file's size, the total unpacked size, and how
//! far an entry may expand relative to its compressed size. Every read is
//! bounded, so an entry whose header lies about its size is still stopped.

use std::io::{Cursor, Read};

use tokio::sync::mpsc;

use super::paths;

/// Most an entry may expand relative to its compressed size.
const MAX_RATIO: u64 = 200;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}

#[derive(Debug)]
pub enum Entry {
    /// A folder (including an empty one).
    Folder(String),
    File {
        path: String,
        bytes: Vec<u8>,
    },
    Skipped {
        path: String,
        reason: String,
        size: Option<u64>,
    },
    /// A limit was reached; nothing after this was read.
    Stopped(String),
}

/// An opened archive: the folder every entry sits in, if they share one
/// (it names the project), and the entries, relative to that folder.
pub struct Opened {
    pub root: Option<String>,
    pub entries: mpsc::Receiver<Entry>,
}

fn entry_path(raw: &zip::read::ZipFile<'_, Cursor<Vec<u8>>>) -> Option<String> {
    let safe = raw.enclosed_name()?;
    let joined = safe
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    paths::normalize(&joined).ok()
}

/// The single top-level folder all entries share, if any.
fn common_root(names: &[String]) -> Option<String> {
    let first = names.first()?.split('/').next()?.to_string();
    let all_inside = names
        .iter()
        .all(|n| n == &first || n.starts_with(&format!("{first}/")));
    let has_children = names.iter().any(|n| n.len() > first.len() + 1);
    (all_inside && has_children).then_some(first)
}

fn strip<'a>(path: &'a str, root: &Option<String>) -> Option<&'a str> {
    match root {
        None => Some(path),
        Some(r) => path
            .strip_prefix(r.as_str())
            .and_then(|rest| rest.strip_prefix('/'))
            .filter(|rest| !rest.is_empty()),
    }
}

/// Open `bytes` as a zip and start streaming its entries.
pub fn open(bytes: Vec<u8>, limits: Limits) -> Result<Opened, String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|_| "this isn't a .zip file Gather can read".to_string())?;
    let names: Vec<String> = (0..archive.len())
        .filter_map(|i| {
            let e = archive.by_index_raw(i).ok()?;
            entry_path(&e)
        })
        .collect();
    let root = common_root(&names);
    let (tx, rx) = mpsc::channel(1);
    let thread_root = root.clone();
    tokio::task::spawn_blocking(move || stream(archive, thread_root, limits, tx));
    Ok(Opened { root, entries: rx })
}

fn stream(
    mut archive: zip::ZipArchive<Cursor<Vec<u8>>>,
    root: Option<String>,
    limits: Limits,
    tx: mpsc::Sender<Entry>,
) {
    let send = |e: Entry| tx.blocking_send(e).is_ok();
    let mut files = 0usize;
    let mut total = 0u64;
    for i in 0..archive.len() {
        let (path, is_dir, is_link, size, compressed) = match archive.by_index_raw(i) {
            Ok(raw) => {
                let Some(path) = entry_path(&raw) else {
                    let name = raw.name().to_string();
                    if !send(Entry::Skipped {
                        path: name,
                        reason: "its path points outside the project".into(),
                        size: None,
                    }) {
                        return;
                    }
                    continue;
                };
                let is_link = raw.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000);
                (
                    path,
                    raw.is_dir(),
                    is_link,
                    raw.size(),
                    raw.compressed_size(),
                )
            }
            Err(_) => continue,
        };
        let Some(rel) = strip(&path, &root).map(str::to_string) else {
            continue; // the root folder itself
        };
        match paths::screen(&rel) {
            paths::Screen::Ignore => continue,
            paths::Screen::Skip(reason) if !is_dir => {
                if !send(Entry::Skipped {
                    path: rel,
                    reason: reason.into(),
                    size: Some(size),
                }) {
                    return;
                }
                continue;
            }
            _ => {}
        }
        if is_dir {
            if !send(Entry::Folder(rel)) {
                return;
            }
            continue;
        }
        if is_link {
            if !send(Entry::Skipped {
                path: rel,
                reason: "links aren't followed".into(),
                size: None,
            }) {
                return;
            }
            continue;
        }
        files += 1;
        if files > limits.max_files {
            send(Entry::Stopped(format!(
                "the project has more than {} files; the rest weren't read",
                limits.max_files
            )));
            return;
        }
        if size > limits.max_file_bytes {
            if !send(Entry::Skipped {
                path: rel,
                reason: format!(
                    "larger than {} MB, the most Gather reads per file",
                    limits.max_file_bytes / (1024 * 1024)
                ),
                size: Some(size),
            }) {
                return;
            }
            continue;
        }
        if size > 1024 * 1024 && size / compressed.max(1) > MAX_RATIO {
            if !send(Entry::Skipped {
                path: rel,
                reason: "it expands far beyond its compressed size; it wasn't opened".into(),
                size: Some(size),
            }) {
                return;
            }
            continue;
        }
        if total.saturating_add(size) > limits.max_total_bytes {
            send(Entry::Stopped(format!(
                "the project unpacks to more than {} MB; the rest wasn't read",
                limits.max_total_bytes / (1024 * 1024)
            )));
            return;
        }
        let read = archive
            .by_index(i)
            .map_err(|_| "it's encrypted or damaged")
            .and_then(|f| {
                let mut bytes = Vec::with_capacity(size.min(limits.max_file_bytes) as usize);
                f.take(limits.max_file_bytes + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|_| "it's damaged")?;
                Ok(bytes)
            });
        let entry = match read {
            Ok(bytes) if bytes.len() as u64 > limits.max_file_bytes => Entry::Skipped {
                path: rel,
                reason: "it's larger than its header says; it wasn't read".into(),
                size: None,
            },
            Ok(bytes) => {
                total = total.saturating_add(bytes.len() as u64);
                Entry::File { path: rel, bytes }
            }
            Err(reason) => Entry::Skipped {
                path: rel,
                reason: reason.into(),
                size: Some(size),
            },
        };
        if !send(entry) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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

    const LIMITS: Limits = Limits {
        max_files: 100,
        max_file_bytes: 4 * 1024 * 1024,
        max_total_bytes: 64 * 1024 * 1024,
    };

    async fn collect(bytes: Vec<u8>, limits: Limits) -> (Option<String>, Vec<Entry>) {
        let mut opened = open(bytes, limits).unwrap();
        let mut out = Vec::new();
        while let Some(e) = opened.entries.recv().await {
            out.push(e);
        }
        (opened.root, out)
    }

    #[tokio::test]
    async fn a_zipped_folder_names_the_project_and_keeps_its_tree() {
        let zip = zip_of(&[
            ("Atlas/", b""),
            ("Atlas/README.md", b"# Atlas"),
            ("Atlas/docs/plan.txt", b"plan"),
            ("Atlas/empty/", b""),
            ("Atlas/.git/HEAD", b"ref"),
            ("Atlas/.env", b"SECRET=1"),
        ]);
        let (root, entries) = collect(zip, LIMITS).await;
        assert_eq!(root.as_deref(), Some("Atlas"));
        let files: Vec<&str> = entries
            .iter()
            .filter_map(|e| match e {
                Entry::File { path, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(files, vec!["README.md", "docs/plan.txt"]);
        assert!(entries
            .iter()
            .any(|e| matches!(e, Entry::Folder(p) if p == "empty")));
        assert!(entries
            .iter()
            .any(|e| matches!(e, Entry::Skipped { path, .. } if path == ".env")));
        assert!(!entries.iter().any(|e| format!("{e:?}").contains(".git")));
    }

    #[tokio::test]
    async fn loose_files_have_no_root() {
        let (root, entries) = collect(zip_of(&[("a.md", b"a"), ("b/c.md", b"c")]), LIMITS).await;
        assert_eq!(root, None);
        assert_eq!(entries.len(), 2);
    }

    #[tokio::test]
    async fn bombs_and_oversized_files_are_not_read() {
        let zeros = vec![0u8; 6 * 1024 * 1024];
        let (_, entries) =
            collect(zip_of(&[("big.txt", &zeros), ("ok.txt", b"fine")]), LIMITS).await;
        assert!(matches!(&entries[0], Entry::Skipped { path, .. } if path == "big.txt"));
        assert!(matches!(&entries[1], Entry::File { path, .. } if path == "ok.txt"));

        let ratio = vec![b'a'; 3 * 1024 * 1024];
        let (_, entries) = collect(zip_of(&[("bomb.txt", &ratio)]), LIMITS).await;
        assert!(matches!(&entries[0], Entry::Skipped { reason, .. } if reason.contains("expands")));
    }

    #[tokio::test]
    async fn the_file_and_size_limits_stop_the_stream() {
        let many: Vec<(String, Vec<u8>)> = (0..5)
            .map(|i| (format!("f{i}.txt"), b"x".to_vec()))
            .collect();
        let refs: Vec<(&str, &[u8])> = many
            .iter()
            .map(|(n, d)| (n.as_str(), d.as_slice()))
            .collect();
        let limits = Limits {
            max_files: 3,
            ..LIMITS
        };
        let (_, entries) = collect(zip_of(&refs), limits).await;
        assert_eq!(
            entries
                .iter()
                .filter(|e| matches!(e, Entry::File { .. }))
                .count(),
            3
        );
        assert!(matches!(entries.last(), Some(Entry::Stopped(_))));
    }

    #[tokio::test]
    async fn entries_that_escape_the_project_are_refused() {
        let (_, entries) = collect(zip_of(&[("../evil.txt", b"x"), ("ok.md", b"y")]), LIMITS).await;
        assert!(matches!(&entries[0], Entry::Skipped { reason, .. } if reason.contains("outside")));
        assert!(matches!(&entries[1], Entry::File { path, .. } if path == "ok.md"));
    }

    #[test]
    fn roots_and_bad_archives() {
        assert!(open(b"not a zip".to_vec(), LIMITS).is_err());
        assert_eq!(
            common_root(&["a/b".into(), "a/c".into()]).as_deref(),
            Some("a")
        );
        assert_eq!(common_root(&["a/b".into(), "c".into()]), None);
        assert_eq!(common_root(&["a".into()]), None);
    }
}
