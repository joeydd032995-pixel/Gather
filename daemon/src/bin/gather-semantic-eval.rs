//! Semantic-safety evaluation report (docs/SEMANTIC-SAFETY.md).
//!
//! Runs the deterministic fixture corpus through the pure safety rules and
//! checks the global invariants. Offline: no database, network or model.
//!
//!   cargo run --bin gather-semantic-eval -- [--fixtures DIR] [--json PATH]
//!
//! Exits non-zero when any scenario or invariant fails.

use std::path::PathBuf;

use gather_daemon::safety::eval;

fn main() {
    let mut fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/semantic");
    let mut json_out: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fixtures" => fixtures = args.next().map(PathBuf::from).unwrap_or(fixtures),
            "--json" => json_out = args.next().map(PathBuf::from),
            "-h" | "--help" => {
                println!("usage: gather-semantic-eval [--fixtures DIR] [--json PATH]");
                return;
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }
    let corpus = match eval::load_dir(&fixtures) {
        Ok(c) if !c.is_empty() => c,
        Ok(_) => {
            eprintln!("no fixtures found in {}", fixtures.display());
            std::process::exit(2);
        }
        Err(e) => {
            eprintln!("failed to load fixtures: {e}");
            std::process::exit(2);
        }
    };
    let report = eval::run(&corpus);
    print!("{}", eval::render(&report));
    if let Some(path) = json_out {
        let body = serde_json::to_string_pretty(&report).expect("report serializes");
        if let Err(e) = std::fs::write(&path, body) {
            eprintln!("failed to write {}: {e}", path.display());
            std::process::exit(2);
        }
    }
    if !report.ok() {
        std::process::exit(1);
    }
}
