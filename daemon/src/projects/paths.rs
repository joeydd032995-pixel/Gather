//! Paths inside an uploaded project: normalized, never escaping the project,
//! and screened for what shouldn't be read at all.

/// Folders left out whole, with why: version-control history, installed
/// dependencies, and what tools rebuild. Each appears in the project's tree
/// as a folder that wasn't read, so nothing disappears without a trace.
const LEFT_OUT_DIRS: &[(&str, &str)] = &[
    (
        ".git",
        "version-control history, not the project's own files",
    ),
    (
        ".hg",
        "version-control history, not the project's own files",
    ),
    (
        ".svn",
        "version-control history, not the project's own files",
    ),
    (
        "node_modules",
        "installed dependencies, which can be reinstalled",
    ),
    (
        "bower_components",
        "installed dependencies, which can be reinstalled",
    ),
    (".venv", "installed dependencies, which can be reinstalled"),
    ("venv", "installed dependencies, which can be reinstalled"),
    ("__pycache__", "made by tools and rebuilt when needed"),
    (".tox", "made by tools and rebuilt when needed"),
    (".mypy_cache", "made by tools and rebuilt when needed"),
    (".pytest_cache", "made by tools and rebuilt when needed"),
    (".ruff_cache", "made by tools and rebuilt when needed"),
    (".gradle", "made by tools and rebuilt when needed"),
    (".next", "made by tools and rebuilt when needed"),
    (".nuxt", "made by tools and rebuilt when needed"),
    (".terraform", "made by tools and rebuilt when needed"),
];
/// Folders of archive-tool metadata (copies of the real files' attributes),
/// left out silently.
const CLUTTER_DIRS: &[&str] = &["__macosx"];
/// Operating-system clutter, left out silently.
const IGNORED_FILES: &[&str] = &[".ds_store", "thumbs.db", "desktop.ini", ".localized"];
/// Files that usually hold keys or passwords. They are recorded as skipped,
/// never read.
const SECRET_NAMES: &[&str] = &[
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    ".npmrc",
    ".pypirc",
    ".netrc",
    ".pgpass",
    ".htpasswd",
    "credentials",
    "credentials.json",
    "secrets.json",
    "secrets.yaml",
    "secrets.yml",
];
const SECRET_EXTENSIONS: &[&str] = &[
    "pem", "key", "p12", "pfx", "jks", "keystore", "kdbx", "ppk", "asc", "gpg",
];
const MAX_PATH_BYTES: usize = 4096;
const MAX_SEGMENT_BYTES: usize = 255;
const MAX_DEPTH: usize = 64;

/// A relative path made safe: `/`-separated, no empty, `.` or `..`
/// segments, nothing absolute. `Err` names why a path can't be used.
pub fn normalize(raw: &str) -> Result<String, &'static str> {
    let unified = raw.replace('\\', "/");
    if unified.starts_with('/') || unified.as_bytes().get(1) == Some(&b':') {
        return Err("absolute paths aren't allowed");
    }
    let mut parts = Vec::new();
    for segment in unified.split('/') {
        match segment {
            "" | "." => continue,
            ".." => return Err("paths may not leave the project"),
            s if s.chars().any(char::is_control) => return Err("the path has control characters"),
            s if s.len() > MAX_SEGMENT_BYTES => return Err("a name in the path is too long"),
            s => parts.push(s),
        }
    }
    if parts.is_empty() {
        return Err("the path is empty");
    }
    if parts.len() > MAX_DEPTH {
        return Err("the path is nested too deeply");
    }
    let path = parts.join("/");
    if path.len() > MAX_PATH_BYTES {
        return Err("the path is too long");
    }
    Ok(path)
}

/// What to do with a (normalized) path before reading it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    /// Read it.
    Read,
    /// Leave it out without a trace (operating-system clutter).
    Ignore,
    /// It sits in `folder`, which is left out whole for `reason`.
    LeftOut {
        folder: String,
        reason: &'static str,
    },
    /// Record it as skipped, with this reason, without reading it.
    Skip(&'static str),
}

/// Why a folder named `name` is left out whole, if it is.
pub fn left_out_reason(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    LEFT_OUT_DIRS
        .iter()
        .find(|(dir, _)| *dir == lower)
        .map(|(_, reason)| *reason)
}

pub fn screen(path: &str) -> Screen {
    let segments: Vec<&str> = path.split('/').collect();
    let (name, folders) = segments
        .split_last()
        .expect("normalized paths are non-empty");
    for (i, folder) in folders.iter().enumerate() {
        if CLUTTER_DIRS.contains(&folder.to_ascii_lowercase().as_str()) {
            return Screen::Ignore;
        }
        if let Some(reason) = left_out_reason(folder) {
            return Screen::LeftOut {
                folder: segments[..=i].join("/"),
                reason,
            };
        }
    }
    let name = name.to_ascii_lowercase();
    if IGNORED_FILES.contains(&name.as_str()) || name.starts_with("._") {
        return Screen::Ignore;
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    if name.starts_with(".env")
        || SECRET_NAMES.contains(&name.as_str())
        || SECRET_EXTENSIONS.contains(&ext)
    {
        return Screen::Skip("looks like it holds keys or passwords, so it wasn't read");
    }
    Screen::Read
}

/// Whether `path` is operating-system or archive-tool clutter, or inside
/// such a folder (`__MACOSX/…`, `.DS_Store`).
pub fn is_clutter(path: &str) -> bool {
    path.split('/')
        .any(|s| CLUTTER_DIRS.contains(&s.to_ascii_lowercase().as_str()))
        || screen(path) == Screen::Ignore
}

/// Whether the file at `path` is a `.zip`, unpacked where it sits.
pub fn is_zip(path: &str) -> bool {
    file_name(path).to_ascii_lowercase().ends_with(".zip")
}

/// Folder paths above `path`, outermost first: `a/b/c.md` → `a`, `a/b`.
pub fn ancestors(path: &str) -> Vec<&str> {
    path.match_indices('/').map(|(i, _)| &path[..i]).collect()
}

pub fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_normalized_and_confined() {
        assert_eq!(normalize("Proj\\docs\\a.md").unwrap(), "Proj/docs/a.md");
        assert_eq!(normalize("./a//b/./c.txt").unwrap(), "a/b/c.txt");
        assert!(normalize("../etc/passwd").is_err());
        assert!(normalize("a/../../b").is_err());
        assert!(normalize("/etc/passwd").is_err());
        assert!(normalize("C:\\Windows\\x").is_err());
        assert!(normalize("a/b\u{0}c").is_err());
        assert!(normalize("").is_err());
        assert!(normalize("./").is_err());
        assert!(normalize(&"a/".repeat(100)).is_err());
    }

    #[test]
    fn tooling_clutter_and_secrets_are_screened() {
        assert_eq!(screen("p/src/main.rs"), Screen::Read);
        assert_eq!(
            screen("p/.git/config"),
            Screen::LeftOut {
                folder: "p/.git".into(),
                reason: "version-control history, not the project's own files"
            }
        );
        assert!(matches!(
            screen("p/web/node_modules/x/node_modules/y.js"),
            Screen::LeftOut { folder, .. } if folder == "p/web/node_modules"
        ));
        assert_eq!(screen("p/.DS_Store"), Screen::Ignore);
        assert_eq!(screen("p/__MACOSX/a/._a.md"), Screen::Ignore);
        assert!(is_clutter("__MACOSX"));
        assert!(is_clutter("__MACOSX/Atlas/._a.md"));
        assert!(is_clutter("Atlas/.DS_Store"));
        assert!(!is_clutter("Atlas/README.md"));
        assert_eq!(
            left_out_reason("Node_Modules"),
            left_out_reason("node_modules")
        );
        assert_eq!(left_out_reason("docs"), None);
        assert!(matches!(screen("p/.env.local"), Screen::Skip(_)));
        assert!(matches!(screen("p/deploy/server.pem"), Screen::Skip(_)));
        assert!(matches!(screen("p/.ssh/id_ed25519"), Screen::Skip(_)));
        // Archives and files of any kind are read (or kept) like the rest.
        assert_eq!(screen("p/old/backup.zip"), Screen::Read);
        assert_eq!(screen("p/tools/setup.exe"), Screen::Read);
        assert!(is_zip("p/old/Backup.ZIP"));
        assert!(!is_zip("p/old/backup.tar.gz"));
    }

    #[test]
    fn ancestors_are_outermost_first() {
        assert_eq!(ancestors("a/b/c.md"), vec!["a", "a/b"]);
        assert!(ancestors("c.md").is_empty());
        assert_eq!(file_name("a/b/c.md"), "c.md");
    }
}
