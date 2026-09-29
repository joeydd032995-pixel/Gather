//! Settings for automatic import: an inbox folder Gather watches, and Claude
//! Code's own session folder. Saved as `import-settings.json` in the app data
//! folder and handed to the daemon as environment variables when it starts
//! (like the AI model settings), so changing them restarts Gather's
//! background service. Everything stays on this computer: the daemon reads
//! these folders and nothing is sent anywhere.

use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

const FILE: &str = "import-settings.json";
/// A folder with more files than this is not "a folder just for imports".
const MAX_EXISTING_FILES: usize = 20;
/// Folders that hold a person's own files: never an inbox, since what is read
/// there is moved away.
const PERSONAL_FOLDERS: &[&str] = &[
    "desktop",
    "documents",
    "downloads",
    "pictures",
    "music",
    "videos",
    "movies",
    "public",
    "onedrive",
    "dropbox",
];

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ImportSettings {
    /// The inbox folder; None = off.
    #[serde(default)]
    pub inbox: Option<String>,
    /// Import Claude Code's sessions automatically.
    #[serde(default)]
    pub claude_code: bool,
}

/// What the Settings page shows.
#[derive(Serialize)]
pub struct ImportView {
    #[serde(flatten)]
    pub settings: ImportSettings,
    /// Where Claude Code's sessions would be read from.
    pub claude_code_dir: Option<String>,
    /// A folder to suggest for the inbox.
    pub suggested_inbox: Option<String>,
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Claude Code keeps sessions in `<config dir>/projects`; the config dir is
/// `CLAUDE_CONFIG_DIR` when set, else `~/.claude`.
pub fn claude_code_dir(home: Option<&Path>) -> Option<PathBuf> {
    let base = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".claude")))?;
    Some(base.join("projects"))
}

pub fn view(data: &Path) -> ImportView {
    let home = home_dir();
    ImportView {
        settings: load(data),
        claude_code_dir: claude_code_dir(home.as_deref()).map(|p| p.display().to_string()),
        suggested_inbox: home.map(|h| h.join("Gather Inbox").display().to_string()),
    }
}

pub fn load(data: &Path) -> ImportSettings {
    fs::read(data.join(FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn saved(data: &Path) -> bool {
    data.join(FILE).is_file()
}

/// The folder as the file system sees it: links followed, for whatever of it
/// exists, and the rest appended. Checks against the home folder and the
/// folders people keep their files in must be made on this, or `Documents/..`
/// or a link to `Documents` would pass as some other folder. A path with `..`
/// in it is refused rather than interpreted.
fn resolve(path: &Path) -> Result<PathBuf, String> {
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("Give the folder's full path, without “..” in it.".to_string());
    }
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(name) => rest.push(name.to_os_string()),
            None => break,
        }
        if !existing.pop() {
            break;
        }
    }
    let base = fs::canonicalize(&existing)
        .map_err(|e| format!("Can't use {}: {e}", existing.display()))?;
    let mut resolved = dunce::simplified(&base).to_path_buf();
    for part in rest.into_iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

/// Check the inbox folder: an absolute path to a folder made for this, not
/// one that holds someone's files (everything read there is moved into a
/// `done` folder inside it).
pub fn check_inbox(path: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    let path = path.trim();
    if path.is_empty() {
        return Err("Choose a folder for the inbox.".to_string());
    }
    let folder = PathBuf::from(path);
    if !folder.is_absolute() || path.contains('\0') {
        return Err("The inbox needs a full folder path.".to_string());
    }
    // Everything below is judged on where the folder really is.
    let folder = resolve(&folder)?;
    let home = home.map(|h| resolve(h).unwrap_or_else(|_| h.to_path_buf()));
    let home = home.as_deref();
    if folder.parent().is_none() || folder.components().count() < 3 {
        return Err("Choose a folder of its own, not a drive or a top-level folder.".to_string());
    }
    let lower_name = folder
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let is_home = home.is_some_and(|h| h == folder);
    let in_home = home.is_some_and(|h| folder.parent() == Some(h));
    if is_home || (in_home && PERSONAL_FOLDERS.contains(&lower_name.as_str())) {
        return Err(format!(
            "“{}” holds your own files, and Gather moves what it reads out of the inbox. \
             Make a folder just for this, such as “Gather Inbox”.",
            folder.display()
        ));
    }
    if folder.is_file() {
        return Err("That is a file, not a folder.".to_string());
    }
    if folder.is_dir() {
        let files = fs::read_dir(&folder)
            .map_err(|e| format!("Can't read that folder: {e}"))?
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .count();
        if files > MAX_EXISTING_FILES {
            return Err(format!(
                "That folder already has {files} files, and Gather moves what it reads out of \
                 the inbox. Choose a folder just for this."
            ));
        }
    }
    Ok(folder)
}

/// Check and save. The daemon picks them up when it next starts.
pub fn save(data: &Path, settings: &ImportSettings) -> Result<ImportSettings, String> {
    let home = home_dir();
    let inbox = match settings.inbox.as_deref().map(str::trim) {
        Some(path) if !path.is_empty() => {
            let folder = check_inbox(path, home.as_deref())?;
            fs::create_dir_all(&folder).map_err(|e| format!("Can't create that folder: {e}"))?;
            Some(folder.display().to_string())
        }
        _ => None,
    };
    if settings.claude_code && claude_code_dir(home.as_deref()).is_none() {
        return Err(
            "Can't tell where Claude Code keeps its sessions on this computer.".to_string(),
        );
    }
    let clean = ImportSettings {
        inbox,
        claude_code: settings.claude_code,
    };
    fs::create_dir_all(data).map_err(|e| format!("saving import settings: {e}"))?;
    let json = serde_json::to_vec_pretty(&clean).map_err(|e| e.to_string())?;
    fs::write(data.join(FILE), json).map_err(|e| format!("saving import settings: {e}"))?;
    Ok(clean)
}

/// Environment for the daemon. Nothing until a choice is saved, so the app's
/// own environment passes through; after that both are always set (empty
/// means off), so turning one off really turns it off.
pub fn daemon_env(data: &Path) -> Vec<(&'static str, String)> {
    if !saved(data) {
        return Vec::new();
    }
    let s = load(data);
    let claude = if s.claude_code {
        claude_code_dir(home_dir().as_deref())
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };
    vec![
        ("GATHER_INBOX_DIR", s.inbox.unwrap_or_default()),
        ("GATHER_CLAUDE_CODE_DIR", claude),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gather-imp-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    #[cfg(unix)]
    fn an_inbox_must_be_a_folder_of_its_own() {
        let home = Path::new("/home/sam");
        assert!(check_inbox("", Some(home)).is_err());
        assert!(check_inbox("relative/dir", Some(home)).is_err());
        assert!(check_inbox("/", Some(home)).is_err());
        assert!(check_inbox("/home", Some(home)).is_err());
        // The home folder and the folders people keep their files in.
        assert!(check_inbox("/home/sam", Some(home)).is_err());
        for name in ["Documents", "Downloads", "Desktop", "downloads"] {
            let err = check_inbox(&format!("/home/sam/{name}"), Some(home)).unwrap_err();
            assert!(err.contains("holds your own files"), "{err}");
        }
        // A folder made for it is fine, wherever it is.
        assert!(check_inbox("/home/sam/Gather Inbox", Some(home)).is_ok());
        assert!(check_inbox("/home/sam/Documents/Gather Inbox", Some(home)).is_ok());
        assert!(check_inbox("/srv/gather-inbox", Some(home)).is_ok());
    }

    #[test]
    #[cfg(unix)]
    fn another_spelling_of_a_protected_folder_is_still_protected() {
        let home = temp("spelling");
        fs::create_dir_all(home.join("Documents")).unwrap();
        let home_str = home.display().to_string();

        // `..` is refused, not interpreted.
        let err = check_inbox(&format!("{home_str}/Documents/.."), Some(&home)).unwrap_err();
        assert!(err.contains(".."), "{err}");
        let err = check_inbox(
            &format!("{home_str}/Gather Inbox/../Documents"),
            Some(&home),
        );
        assert!(err.is_err());

        // A link to Documents, or to the home folder, is that folder.
        std::os::unix::fs::symlink(home.join("Documents"), home.join("inbox-link")).unwrap();
        let err = check_inbox(&format!("{home_str}/inbox-link"), Some(&home)).unwrap_err();
        assert!(err.contains("holds your own files"), "{err}");
        std::os::unix::fs::symlink(&home, home.join("home-link")).unwrap();
        let err = check_inbox(&format!("{home_str}/home-link"), Some(&home)).unwrap_err();
        assert!(err.contains("holds your own files"), "{err}");
        // A link inside a protected folder to somewhere else is fine, and the
        // saved path is where it really is.
        let elsewhere = temp("spelling-elsewhere");
        std::os::unix::fs::symlink(&elsewhere, home.join("to-elsewhere")).unwrap();
        let ok = check_inbox(&format!("{home_str}/to-elsewhere"), Some(&home)).unwrap();
        assert_eq!(ok, fs::canonicalize(&elsewhere).unwrap());
        // A folder that doesn't exist yet is judged on its real parent.
        std::os::unix::fs::symlink(home.join("Documents"), home.join("docs-link")).unwrap();
        let ok = check_inbox(&format!("{home_str}/docs-link/Gather Inbox"), Some(&home)).unwrap();
        assert_eq!(
            ok,
            fs::canonicalize(home.join("Documents"))
                .unwrap()
                .join("Gather Inbox")
        );
        let _ = fs::remove_dir_all(home);
        let _ = fs::remove_dir_all(elsewhere);
    }

    #[test]
    fn a_folder_full_of_files_is_refused() {
        let dir = temp("full");
        for i in 0..=MAX_EXISTING_FILES {
            fs::write(dir.join(format!("f{i}.txt")), "x").unwrap();
        }
        let err = check_inbox(&dir.display().to_string(), None).unwrap_err();
        assert!(err.contains("already has"), "{err}");
        let empty = temp("empty");
        assert!(check_inbox(&empty.display().to_string(), None).is_ok());
        let _ = fs::remove_dir_all(dir);
        let _ = fs::remove_dir_all(empty);
    }

    #[test]
    fn nothing_is_passed_on_until_a_choice_is_saved() {
        let data = temp("data-none");
        assert!(daemon_env(&data).is_empty());
        assert_eq!(load(&data), ImportSettings::default());
        let _ = fs::remove_dir_all(data);
    }

    #[test]
    fn saved_choices_become_the_daemons_environment() {
        let data = temp("data-save");
        let inbox = temp("inbox-save");
        let saved = save(
            &data,
            &ImportSettings {
                inbox: Some(inbox.display().to_string()),
                claude_code: true,
            },
        )
        .unwrap();
        assert_eq!(
            saved.inbox.as_deref(),
            Some(inbox.display().to_string().as_str())
        );
        let env = daemon_env(&data);
        assert_eq!(env[0], ("GATHER_INBOX_DIR", inbox.display().to_string()));
        assert_eq!(env[1].0, "GATHER_CLAUDE_CODE_DIR");
        assert!(env[1].1.ends_with("projects"), "{env:?}");

        // Both off: still passed, empty, so the daemon turns them off.
        save(&data, &ImportSettings::default()).unwrap();
        assert_eq!(
            daemon_env(&data),
            vec![
                ("GATHER_INBOX_DIR", String::new()),
                ("GATHER_CLAUDE_CODE_DIR", String::new())
            ]
        );
        let _ = fs::remove_dir_all(data);
        let _ = fs::remove_dir_all(inbox);
    }

    #[test]
    #[cfg(unix)]
    fn claude_codes_folder_follows_its_config_dir() {
        let home = Path::new("/home/sam");
        // (CLAUDE_CONFIG_DIR is read from the environment; without it the
        // default under the home folder is used.)
        if std::env::var_os("CLAUDE_CONFIG_DIR").is_none() {
            assert_eq!(
                claude_code_dir(Some(home)),
                Some(PathBuf::from("/home/sam/.claude/projects"))
            );
            assert_eq!(claude_code_dir(None), None);
        }
    }
}
