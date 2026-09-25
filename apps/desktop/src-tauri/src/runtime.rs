//! Runs Gather's local stack for the packaged app: the bundled PostgreSQL
//! (with pgvector) and the `gather-daemon` sidecar. Everything stays on this
//! machine: Postgres listens on 127.0.0.1 only, with no Unix socket, and the
//! daemon keeps its loopback-only defaults.
//!
//! Development builds carry no bundled Postgres, so the supervisor stands
//! aside ([`Status::Unmanaged`]) and the UI talks to whatever daemon the
//! developer runs, as before. A daemon already answering on the API port
//! (e.g. left over from a crashed session) is reused rather than duplicated.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::memory::{self, Profile};

/// The daemon's REST address (its default, which the UI also uses).
const DAEMON_ADDR: &str = "127.0.0.1:7601";
/// Postgres port for the bundled cluster; away from 5432 so an existing
/// system Postgres is never touched. Override with GATHER_PG_PORT.
const DEFAULT_PG_PORT: u16 = 7603;
/// The PostgreSQL major version the bundle ships; a data directory from any
/// other major version needs pg_upgrade, never a silent start.
const PG_MAJOR: &str = "16";
const DB_USER: &str = "gather";
const DB_NAME: &str = "gather";
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(90);
const DAEMON_STOP_GRACE: Duration = Duration::from_secs(10);
/// Where the spawned daemon's PID is recorded, in the app data dir.
const DAEMON_PID_FILE: &str = "daemon.pid";
/// A log bigger than this is started afresh on the next launch.
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// What the UI shows while the stack comes up.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    Starting {
        step: String,
    },
    Ready,
    /// Not managed by the app: a development build, or a daemon that was
    /// already running when the app started.
    Unmanaged,
    Failed {
        message: String,
        log_dir: String,
    },
}

/// Where the supervisor finds its binaries and keeps its data.
#[derive(Clone)]
pub struct Paths {
    /// Bundled PostgreSQL install (bin/, lib/, share/).
    pub postgres: PathBuf,
    /// The daemon sidecar executable.
    pub daemon: PathBuf,
    /// App data directory: pgdata/, logs/, the database password.
    pub data: PathBuf,
}

/// The daemon this app is responsible for stopping.
enum Daemon {
    /// Started by this session.
    Spawned(Child),
    /// Started by an earlier session that crashed, and still running.
    Adopted(u32),
}

#[derive(Default)]
pub struct Runtime {
    status: Mutex<Option<Status>>,
    /// Kept so the stack can be brought back up (see `resume`).
    paths: Mutex<Option<Paths>>,
    daemon: Mutex<Option<Daemon>>,
    /// Set once this process started (or adopted) the Postgres cluster.
    pg_data: Mutex<Option<PathBuf>>,
    postgres_bin: Mutex<Option<PathBuf>>,
    /// Set by `stop`: a start still in progress must not launch anything more.
    stopping: AtomicBool,
}

impl Runtime {
    pub fn status(&self) -> Status {
        self.status
            .lock()
            .expect("status lock")
            .clone()
            .unwrap_or(Status::Starting {
                step: "Starting".to_string(),
            })
    }

    fn set(&self, status: Status) {
        *self.status.lock().expect("status lock") = Some(status);
    }

    /// Report progress, or give up if the app is quitting meanwhile.
    fn step(&self, step: &str) -> Result<(), String> {
        if self.stopping.load(Ordering::SeqCst) {
            return Err("Gather is shutting down".to_string());
        }
        self.set(Status::Starting {
            step: step.to_string(),
        });
        Ok(())
    }

    /// Bring the stack up. Blocking: call from a background thread.
    pub fn start(&self, paths: &Paths) {
        *self.paths.lock().expect("lock") = Some(paths.clone());
        self.set(Status::Starting {
            step: "Starting".to_string(),
        });
        let logs = paths.data.join("logs");
        match self.try_start(paths, &logs) {
            Ok(status) => self.set(status),
            Err(message) => self.set(Status::Failed {
                message,
                log_dir: logs.display().to_string(),
            }),
        }
    }

    fn try_start(&self, paths: &Paths, logs: &Path) -> Result<Status, String> {
        let bin = paths.postgres.join("bin");
        let bundled = exe(&bin, "pg_ctl").exists() && paths.daemon.exists();
        let pg_data = paths.data.join("pgdata");
        if daemon_healthy() {
            // Ours, left running by a session that crashed: take it back so
            // quitting (or an update) stops it. Anything else, e.g. a daemon
            // the user runs with Docker, is theirs to manage.
            if bundled && self.adopt(paths, &bin, &pg_data) {
                return Ok(Status::Ready);
            }
            return Ok(Status::Unmanaged);
        }
        if !bundled {
            return Ok(Status::Unmanaged);
        }
        fs::create_dir_all(logs).map_err(|e| format!("creating {}: {e}", logs.display()))?;

        let port = pg_port()?;
        let password = database_password(&paths.data)?;
        let lib = paths.postgres.join("lib");

        // An existing data directory is never modified here, even if it looks
        // incomplete: check_major_version refuses it instead.
        if !pg_data.exists() {
            self.step("Setting up your private database (first run)")?;
            init_cluster(&bin, &lib, &pg_data, &paths.data, &password, port)?;
        }
        check_major_version(&pg_data)?;

        let profile = memory::current().profile;
        self.step("Starting the database")?;
        *self.postgres_bin.lock().expect("lock") = Some(bin.clone());
        *self.pg_data.lock().expect("lock") = Some(pg_data.clone());
        start_postgres(&bin, &lib, &pg_data, logs, port, profile)?;
        ensure_database(&bin, &lib, &password, port)?;

        self.step("Starting Gather")?;
        let url = format!("postgres://{DB_USER}:{password}@127.0.0.1:{port}/{DB_NAME}");
        let mut child = spawn_daemon(&paths.daemon, &url, logs, profile)?;
        {
            let mut slot = self.daemon.lock().expect("lock");
            // `stop` may have run between the last step and the spawn.
            if self.stopping.load(Ordering::SeqCst) {
                drop(slot);
                stop_child(&mut child);
                return Err("Gather is shutting down".to_string());
            }
            // Recorded so a later session can adopt it after a crash.
            let _ = fs::write(paths.data.join(DAEMON_PID_FILE), child.id().to_string());
            *slot = Some(Daemon::Spawned(child));
        }

        let deadline = Instant::now() + DAEMON_START_TIMEOUT;
        while Instant::now() < deadline {
            if daemon_healthy() {
                return Ok(Status::Ready);
            }
            if let Some(Daemon::Spawned(child)) = self.daemon.lock().expect("lock").as_mut() {
                if let Ok(Some(exit)) = child.try_wait() {
                    return Err(format!(
                        "Gather's background service stopped during start-up ({exit}). \
                         Details are in daemon.log."
                    ));
                }
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Err("Gather's background service did not become ready in time. \
             Details are in daemon.log."
            .to_string())
    }

    /// Stop what this process started: the daemon first (it holds database
    /// connections), then Postgres. Safe to call more than once.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        match self.daemon.lock().expect("lock").take() {
            Some(Daemon::Spawned(mut child)) => stop_child(&mut child),
            Some(Daemon::Adopted(pid)) => stop_pid(pid),
            None => {}
        }
        if let Some(paths) = self.paths.lock().expect("lock").as_ref() {
            let _ = fs::remove_file(paths.data.join(DAEMON_PID_FILE));
        }
        let bin = self.postgres_bin.lock().expect("lock").take();
        let data = self.pg_data.lock().expect("lock").take();
        if let (Some(bin), Some(data)) = (bin, data) {
            let lib = bin.parent().map(|p| p.join("lib")).unwrap_or_default();
            let _ = pg_command(&bin, &lib, "pg_ctl")
                .arg("stop")
                .arg("-D")
                .arg(&data)
                .args(["-m", "fast", "-w"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    /// Bring the stack back up after `stop`, e.g. when an update failed to
    /// install after the stack was stopped for it.
    pub fn resume(self: &Arc<Self>) {
        let Some(paths) = self.paths.lock().expect("lock").clone() else {
            return;
        };
        self.stopping.store(false, Ordering::SeqCst);
        self.set(Status::Starting {
            step: "Restarting".to_string(),
        });
        let runtime = Arc::clone(self);
        std::thread::spawn(move || runtime.start(&paths));
    }

    /// Take over a daemon (and its database) that an earlier session of this
    /// app started, identified by the PID it recorded and the process name.
    fn adopt(&self, paths: &Paths, bin: &Path, pg_data: &Path) -> bool {
        let pid = fs::read_to_string(paths.data.join(DAEMON_PID_FILE))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        let Some(pid) = pid.filter(|&pid| is_daemon_process(pid)) else {
            return false;
        };
        *self.daemon.lock().expect("lock") = Some(Daemon::Adopted(pid));
        if pg_data.join("PG_VERSION").exists() {
            *self.postgres_bin.lock().expect("lock") = Some(bin.to_path_buf());
            *self.pg_data.lock().expect("lock") = Some(pg_data.to_path_buf());
        }
        true
    }
}

fn exe(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

fn pg_port() -> Result<u16, String> {
    match std::env::var("GATHER_PG_PORT") {
        Ok(v) => v
            .parse()
            .map_err(|_| format!("GATHER_PG_PORT is not a valid port: {v}")),
        Err(_) => Ok(DEFAULT_PG_PORT),
    }
}

/// A command for one of the bundled Postgres tools, with the dynamic loader
/// pointed at the bundle's own lib/ (it carries the OpenSSL that pgcrypto
/// needs) and no console window on Windows.
fn pg_command(bin: &Path, lib: &Path, tool: &str) -> Command {
    let mut cmd = Command::new(exe(bin, tool));
    #[cfg(target_os = "linux")]
    cmd.env("LD_LIBRARY_PATH", lib);
    #[cfg(target_os = "macos")]
    cmd.env("DYLD_LIBRARY_PATH", lib);
    #[cfg(windows)]
    let _ = lib; // DLLs next to the executables are found without help.
    hide_window(&mut cmd);
    cmd.stdin(Stdio::null());
    cmd
}

#[cfg(windows)]
fn hide_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_window(_cmd: &mut Command) {}

/// Run a command to completion; on failure, report its output.
fn run(mut cmd: Command, what: &str) -> Result<(), String> {
    let out = cmd.output().map_err(|e| format!("{what}: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    Err(format!(
        "{what} failed ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

/// The database password lives in a file readable only by this user, next to
/// the data directory it protects: anyone able to read the file can read the
/// data files directly anyway, and unlike a keychain entry it causes no
/// access prompts when an unsigned app is updated.
fn database_password(data: &Path) -> Result<String, String> {
    let path = data.join("db-password");
    if let Ok(existing) = fs::read_to_string(&path) {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return Ok(existing);
        }
    }
    fs::create_dir_all(data).map_err(|e| format!("creating {}: {e}", data.display()))?;
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).map_err(|e| format!("generating a password: {e}"))?;
    let password: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    write_private(&path, &password)?;
    Ok(password)
}

fn write_private(path: &Path, contents: &str) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("writing {}: {e}", path.display()))?;
    file.write_all(contents.as_bytes())
        .map_err(|e| format!("writing {}: {e}", path.display()))
}

fn init_cluster(
    bin: &Path,
    lib: &Path,
    pg_data: &Path,
    data: &Path,
    password: &str,
    port: u16,
) -> Result<(), String> {
    // Built aside and moved into place only when complete, so the real data
    // directory is never a half-initialized cluster. A leftover staging dir
    // is an earlier attempt that never held any data.
    let staging = data.join("pgdata.init");
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(|e| format!("clearing {}: {e}", staging.display()))?;
    }
    let pwfile = data.join("db-password.init");
    write_private(&pwfile, password)?;
    let mut cmd = pg_command(bin, lib, "initdb");
    cmd.arg("-D")
        .arg(&staging)
        .args(["-U", DB_USER, "--auth=scram-sha-256", "-E", "UTF8"])
        .arg("--no-instructions")
        .arg(format!("--pwfile={}", pwfile.display()));
    let result = run(cmd, "initializing the database");
    let _ = fs::remove_file(&pwfile);
    result?;

    // Loopback TCP only, no Unix socket: nothing else on the machine gets a
    // path in, and the password is required even locally.
    let conf = format!(
        "\n# Gather desktop\nlisten_addresses = '127.0.0.1'\nport = {port}\n\
         unix_socket_directories = ''\nmax_connections = 32\n"
    );
    OpenOptions::new()
        .append(true)
        .open(staging.join("postgresql.conf"))
        .and_then(|mut f| f.write_all(conf.as_bytes()))
        .map_err(|e| format!("configuring the database: {e}"))?;
    fs::rename(&staging, pg_data).map_err(|e| format!("finishing database setup: {e}"))
}

fn check_major_version(pg_data: &Path) -> Result<(), String> {
    let Ok(version) = fs::read_to_string(pg_data.join("PG_VERSION")) else {
        return Err(format!(
            "The database folder {} is incomplete or damaged. Gather has left it untouched: \
             restore it from a backup, or move it aside to start with an empty database.",
            pg_data.display()
        ));
    };
    if version.trim() == PG_MAJOR {
        return Ok(());
    }
    Err(format!(
        "Your data was created by PostgreSQL {} but this version of Gather bundles \
         PostgreSQL {PG_MAJOR}. Your data is untouched; see docs/INSTALL.md for upgrading.",
        version.trim()
    ))
}

fn start_postgres(
    bin: &Path,
    lib: &Path,
    pg_data: &Path,
    logs: &Path,
    port: u16,
    profile: Profile,
) -> Result<(), String> {
    // Already running (e.g. the app crashed last time): restart it, since it
    // may have been started with another memory profile's settings.
    let running = pg_command(bin, lib, "pg_ctl")
        .arg("status")
        .arg("-D")
        .arg(pg_data)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if running {
        let stopped = pg_command(bin, lib, "pg_ctl")
            .arg("stop")
            .arg("-D")
            .arg(pg_data)
            .args(["-m", "fast", "-w", "-t", "60"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| format!("restarting the database: {e}"))?;
        if !stopped.success() {
            return Err(format!(
                "restarting the database failed ({stopped}). Details are in postgres.log."
            ));
        }
    }
    let log = logs.join("postgres.log");
    trim_log(&log);
    // pg_ctl's own output goes to a file, never a pipe: on Windows the server
    // it launches inherits these handles, so a pipe would never reach EOF and
    // reading it (as `Command::output` does) would wait forever.
    let ctl_log_path = logs.join("pg_ctl.log");
    let ctl_log = File::create(&ctl_log_path)
        .map_err(|e| format!("opening {}: {e}", ctl_log_path.display()))?;
    let ctl_err = ctl_log
        .try_clone()
        .map_err(|e| format!("opening {}: {e}", ctl_log_path.display()))?;
    // Server options: the port, plus the memory profile's settings (none of
    // which contain spaces, so pg_ctl passes them through unquoted).
    let mut options = format!("-p {port}");
    for setting in profile.postgres_settings() {
        options.push_str(" -c ");
        options.push_str(setting);
    }
    let status = pg_command(bin, lib, "pg_ctl")
        .arg("start")
        .arg("-D")
        .arg(pg_data)
        .arg("-l")
        .arg(&log)
        .args(["-w", "-t", "60", "-o"])
        .arg(options)
        .stdout(ctl_log)
        .stderr(ctl_err)
        .status()
        .map_err(|e| format!("starting the database: {e}"))?;
    if status.success() {
        return Ok(());
    }
    let detail = fs::read_to_string(&ctl_log_path).unwrap_or_default();
    Err(format!(
        "starting the database failed ({status}): {}\n\
         If port {port} is taken, set GATHER_PG_PORT to a free port.",
        detail.trim()
    ))
}

/// initdb creates only the `postgres` database; the daemon uses its own.
fn ensure_database(bin: &Path, lib: &Path, password: &str, port: u16) -> Result<(), String> {
    let psql = |sql: &str| {
        let mut cmd = pg_command(bin, lib, "psql");
        cmd.env("PGPASSWORD", password)
            .args(["-h", "127.0.0.1", "-p", &port.to_string(), "-U", DB_USER])
            .args(["-d", "postgres", "-v", "ON_ERROR_STOP=1", "-qAt", "-c", sql]);
        cmd
    };
    let exists = psql(&format!(
        "SELECT 1 FROM pg_database WHERE datname = '{DB_NAME}'"
    ))
    .output()
    .map_err(|e| format!("checking the database: {e}"))?;
    if String::from_utf8_lossy(&exists.stdout).trim() == "1" {
        return Ok(());
    }
    run(
        psql(&format!("CREATE DATABASE {DB_NAME}")),
        "creating the database",
    )
}

fn spawn_daemon(
    daemon: &Path,
    database_url: &str,
    logs: &Path,
    profile: Profile,
) -> Result<Child, String> {
    let log_path = logs.join("daemon.log");
    trim_log(&log_path);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("opening {}: {e}", log_path.display()))?;
    let err_log = log
        .try_clone()
        .map_err(|e| format!("opening {}: {e}", log_path.display()))?;
    let mut cmd = Command::new(daemon);
    // Always the resolved profile: the daemon doesn't understand `auto`.
    cmd.env("DATABASE_URL", database_url)
        .env("GATHER_MEMORY_PROFILE", profile.as_str())
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(err_log);
    // The API token lives in the OS keychain unless the user has chosen
    // otherwise (e.g. a minimal Linux desktop without a Secret Service).
    if std::env::var_os("GATHER_AUTH_MODE").is_none() {
        cmd.env("GATHER_AUTH_MODE", "keychain");
    }
    hide_window(&mut cmd);
    cmd.spawn()
        .map_err(|e| format!("starting {}: {e}", daemon.display()))
}

fn trim_log(path: &Path) {
    if fs::metadata(path)
        .map(|m| m.len() > MAX_LOG_BYTES)
        .unwrap_or(false)
    {
        let _ = File::create(path);
    }
}

/// Ask the daemon to shut down cleanly, then insist.
fn stop_child(child: &mut Child) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status();
        let deadline = Instant::now() + DAEMON_STOP_GRACE;
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Whether `pid` is a running gather-daemon (guards against a recorded PID
/// that has since been reused by an unrelated process).
fn is_daemon_process(pid: u32) -> bool {
    #[cfg(unix)]
    let out = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output();
    #[cfg(windows)]
    let out = {
        let mut cmd = Command::new("tasklist");
        cmd.args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"]);
        hide_window(&mut cmd);
        cmd.output()
    };
    out.map(|o| {
        String::from_utf8_lossy(&o.stdout)
            .trim()
            .trim_matches('"')
            .split('"')
            .next()
            .unwrap_or("")
            .trim_end_matches(".exe")
            .ends_with("gather-daemon")
    })
    .unwrap_or(false)
}

/// Stop an adopted daemon: ask first, then insist.
fn stop_pid(pid: u32) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
        let deadline = Instant::now() + DAEMON_STOP_GRACE;
        while Instant::now() < deadline {
            if !is_daemon_process(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }
    #[cfg(windows)]
    {
        let mut cmd = Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string(), "/F"]);
        hide_window(&mut cmd);
        let _ = cmd.status();
    }
}

/// `GET /healthz` over plain loopback TCP (no HTTP client dependency).
fn daemon_healthy() -> bool {
    let addr: SocketAddr = DAEMON_ADDR.parse().expect("valid daemon address");
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    if stream
        .write_all(b"GET /healthz HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut head = [0u8; 64];
    let n = stream.read(&mut head).unwrap_or(0);
    let head = String::from_utf8_lossy(&head[..n]);
    head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200")
}
