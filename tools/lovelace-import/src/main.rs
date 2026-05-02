//! Lovelace → hanui dashboard YAML importer (CLI binary).
//!
//! Phase 6 sub-phase 6c (TASK-111). Wraps the [`lovelace_import::convert`]
//! library entry point with a small argument parser. We deliberately do NOT
//! pull in `clap` here — the importer is a single-shot dev-time tool and the
//! flag surface (`--input`, `--output`, `--force`, `--stdout`, `--help`) is
//! small enough that hand-parsing avoids a 50+ KiB dependency tree on a
//! dev-only crate.
//!
//! # Output path policy
//!
//! Per `locked_decisions.lovelace_import_output_path` in
//! `docs/plans/2026-04-30-phase-6-advanced-widgets.md`:
//! - Default output path is `dashboard.lovelace-import.yaml` in cwd.
//! - `--output <path>` overrides the default; the importer still REFUSES to
//!   write to a file whose basename equals `dashboard.yaml` (the production
//!   filename), regardless of `--force`. The user's production file must
//!   never be silently overwritten by a dev-time migration helper.
//! - `--force` bypasses the existing-file check for any other path.
//! - `--stdout` writes the converted YAML to stdout instead of a file. The
//!   UNMAPPED log still goes to stderr in this mode.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use lovelace_import::{convert, Conversion, ImportError};

const DEFAULT_OUTPUT: &str = "dashboard.lovelace-import.yaml";
const PRODUCTION_FILENAME: &str = "dashboard.yaml";

/// Parsed command-line arguments.
struct Args {
    input: Option<PathBuf>,
    output: PathBuf,
    force: bool,
    stdout: bool,
}

fn print_usage() {
    eprintln!(
        "Usage: lovelace-import [--input <path>] [--output <path>] [--force] [--stdout]\n\
         \n\
         Convert a Home Assistant Lovelace dashboard YAML to hanui dashboard YAML.\n\
         \n\
         Flags:\n  \
         --input <path>    Lovelace YAML to read (default: stdin).\n  \
         --output <path>   Where to write hanui YAML (default: ./{DEFAULT_OUTPUT}).\n  \
         --force           Overwrite the output file if it already exists.\n  \
         --stdout          Write hanui YAML to stdout instead of a file.\n  \
         --help            Print this help and exit.\n\
         \n\
         The importer NEVER writes to a file whose basename is `{PRODUCTION_FILENAME}` \
         (the production config), regardless of --force.",
    );
}

fn parse_args() -> Result<Args, String> {
    let mut input: Option<PathBuf> = None;
    let mut output: PathBuf = PathBuf::from(DEFAULT_OUTPUT);
    let mut force = false;
    let mut stdout = false;

    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw.len() {
        let arg = &raw[i];
        match arg.as_str() {
            "--input" => {
                i += 1;
                let v = raw
                    .get(i)
                    .ok_or_else(|| "--input requires a path argument".to_string())?;
                input = Some(PathBuf::from(v));
            }
            "--output" => {
                i += 1;
                let v = raw
                    .get(i)
                    .ok_or_else(|| "--output requires a path argument".to_string())?;
                output = PathBuf::from(v);
            }
            "--force" => force = true,
            "--stdout" => stdout = true,
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => {
                return Err(format!("unknown argument: {other}"));
            }
        }
        i += 1;
    }

    Ok(Args {
        input,
        output,
        force,
        stdout,
    })
}

/// Read the Lovelace YAML from `--input <path>` or from stdin.
fn read_input(args: &Args) -> Result<String, String> {
    match &args.input {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|e| format!("could not read --input {}: {e}", path.display())),
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| format!("could not read stdin: {e}"))?;
            Ok(buf)
        }
    }
}

/// Returns true when `path`'s basename equals `PRODUCTION_FILENAME` — used to
/// refuse overwriting the production dashboard file regardless of `--force`.
fn would_overwrite_production_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s == PRODUCTION_FILENAME)
}

/// Compose the YAML payload that gets written to disk / stdout: the converted
/// dashboard plus a trailing `# UNMAPPED:` comment block when the importer
/// could not map every Lovelace card. The block is appended (not interleaved)
/// so the comment lines do not interfere with hanui's YAML loader.
fn render_with_unmapped(c: &Conversion) -> String {
    if c.unmapped.is_empty() {
        return c.yaml.clone();
    }
    let mut out = c.yaml.clone();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("# UNMAPPED: the following Lovelace cards have no hanui mapping;\n");
    out.push_str("# fix these by hand before deploying.\n");
    for line in &c.unmapped {
        out.push_str("# UNMAPPED: ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// CLI entry point.
fn run() -> Result<(), String> {
    let args = parse_args()?;

    let input = read_input(&args)?;
    let conversion = convert(&input).map_err(|e| match e {
        ImportError::InputParse(s) => format!("invalid Lovelace YAML: {s}"),
        ImportError::InputShape(s) => format!("unexpected Lovelace shape: {s}"),
        ImportError::OutputValidation(s) => format!("importer produced invalid hanui YAML:\n{s}"),
        ImportError::OutputSerialise(s) => format!("could not emit hanui YAML: {s}"),
    })?;

    let payload = render_with_unmapped(&conversion);

    // UNMAPPED log always goes to stderr, regardless of --stdout, so it is
    // visible during scripting use.
    if !conversion.unmapped.is_empty() {
        eprintln!(
            "lovelace-import: {} unmapped card(s):",
            conversion.unmapped.len()
        );
        for line in &conversion.unmapped {
            eprintln!("  - {line}");
        }
    }

    if args.stdout {
        print!("{payload}");
        return Ok(());
    }

    if would_overwrite_production_file(&args.output) {
        return Err(format!(
            "refusing to write to '{}': basename is the production filename '{PRODUCTION_FILENAME}'.\n\
             Pass --output <other-path> instead.",
            args.output.display(),
        ));
    }

    if args.output.exists() && !args.force {
        return Err(format!(
            "output file already exists: '{}'. Pass --force to overwrite.",
            args.output.display(),
        ));
    }

    std::fs::write(&args.output, &payload)
        .map_err(|e| format!("could not write {}: {e}", args.output.display()))?;
    eprintln!("lovelace-import: wrote {}", args.output.display());
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("lovelace-import: {msg}");
            ExitCode::from(1)
        }
    }
}
