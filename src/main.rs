mod exif;
mod info;
mod strip;

use clap::Parser;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};

struct Opts {
    recursive: bool,
    verbose: bool,
    info: bool,
    mode: strip::Mode,
}

/// Outcome of processing a single regular file.
enum Outcome {
    Stripped,
    Clean,
    Unsupported,
    Listed,
}

/// Strip metadata from images, invoked like rm.
///
/// Keeps orientation, resolution and colour space by default so photos still
/// display correctly; pass -a to remove those as well.
#[derive(Parser)]
#[command(name = "metarm", version, author, arg_required_else_help = true)]
struct Cli {
    /// Process directories recursively
    #[arg(short = 'r', long, short_alias = 'R')]
    recursive: bool,

    /// Accepted for rm compatibility; has no effect
    #[arg(short = 'f', long)]
    force: bool,

    /// Also remove orientation, resolution and colour-space tags
    #[arg(short = 'a', long)]
    all: bool,

    /// List the metadata in each file (and what would be stripped) without changing anything
    #[arg(short = 'i', long)]
    info: bool,

    /// Print each file as it is processed; with -i, show values in full
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Files to strip (directories require -r)
    #[arg(value_name = "FILE", required = true)]
    files: Vec<PathBuf>,
}

fn main() {
    let cli = Cli::parse();
    let _ = cli.force;
    let opts = Opts {
        recursive: cli.recursive,
        verbose: cli.verbose,
        info: cli.info,
        mode: if cli.all {
            strip::Mode::All
        } else {
            strip::Mode::KeepDisplay
        },
    };

    let mut ok = true;
    for path in &cli.files {
        ok &= process_arg(path, &opts);
    }
    process::exit(if ok { 0 } else { 1 });
}

fn report_err(path: &Path, msg: impl std::fmt::Display) {
    eprintln!("metarm: {}: {}", path.display(), msg);
}

fn io_msg(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::NotFound => "No such file or directory".into(),
        io::ErrorKind::PermissionDenied => "Permission denied".into(),
        _ => e.to_string(),
    }
}

/// Handles a path named on the command line. Symlinks are followed here
/// (the user pointed at them explicitly); they are not followed during recursion.
fn process_arg(path: &Path, opts: &Opts) -> bool {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            report_err(path, io_msg(&e));
            return false;
        }
    };

    if meta.is_dir() {
        if !opts.recursive {
            report_err(path, "is a directory");
            return false;
        }
        return walk(path, opts);
    }

    // Resolve a symlink so we rewrite the target, not replace the link.
    let target = match fs::canonicalize(path) {
        Ok(p) => p,
        Err(e) => {
            report_err(path, io_msg(&e));
            return false;
        }
    };
    match process_file(&target, path, &meta, opts) {
        Ok(Outcome::Unsupported) => {
            report_err(path, "unsupported file format");
            false
        }
        Ok(outcome) => {
            log(path, &outcome, opts);
            true
        }
        Err(e) => {
            report_err(path, e);
            false
        }
    }
}

fn walk(dir: &Path, opts: &Opts) -> bool {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            report_err(dir, io_msg(&e));
            return false;
        }
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut ok = true;
    for entry in entries {
        match entry {
            Ok(e) => paths.push(e.path()),
            Err(e) => {
                report_err(dir, io_msg(&e));
                ok = false;
            }
        }
    }
    paths.sort();

    for path in paths {
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                report_err(&path, io_msg(&e));
                ok = false;
                continue;
            }
        };
        let ft = meta.file_type();
        if ft.is_dir() {
            ok &= walk(&path, opts);
        } else if ft.is_file() {
            match process_file(&path, &path, &meta, opts) {
                Ok(outcome) => log(&path, &outcome, opts),
                Err(e) => {
                    report_err(&path, e);
                    ok = false;
                }
            }
        }
        // Symlinks, sockets, fifos, devices: skipped.
    }
    ok
}

fn log(path: &Path, outcome: &Outcome, opts: &Opts) {
    if !opts.verbose {
        return;
    }
    let what = match outcome {
        Outcome::Listed => return,
        Outcome::Stripped => "stripped",
        Outcome::Clean => "no metadata",
        Outcome::Unsupported => "skipped (unsupported format)",
    };
    println!("{}: '{}'", what, path.display());
}

/// `path` is the file to read and write; `shown` is how to refer to it in output
/// (they differ when the user named a symlink).
fn process_file(
    path: &Path,
    shown: &Path,
    meta: &fs::Metadata,
    opts: &Opts,
) -> Result<Outcome, String> {
    let data = fs::read(path).map_err(|e| io_msg(&e))?;
    let format = match strip::detect(&data) {
        Some(f) => f,
        None => return Ok(Outcome::Unsupported),
    };
    if opts.info {
        let items =
            info::inspect(format, &data, opts.mode).map_err(|e| format!("{} ({})", e, format))?;
        print_info(shown, format, &items, opts.verbose);
        return Ok(Outcome::Listed);
    }
    let cleaned =
        strip::strip(format, &data, opts.mode).map_err(|e| format!("{} ({})", e, format))?;
    if cleaned == data {
        return Ok(Outcome::Clean);
    }
    write_atomic(path, &cleaned, meta).map_err(|e| io_msg(&e))?;
    Ok(Outcome::Stripped)
}

fn print_info(path: &Path, format: strip::Format, items: &[info::Item], full: bool) {
    static PRINTED: AtomicBool = AtomicBool::new(false);
    if PRINTED.swap(true, Ordering::Relaxed) {
        println!();
    }
    if items.is_empty() {
        println!("{}: {}, no metadata", path.display(), format);
        return;
    }
    println!("{}: {}", path.display(), format);
    let gw = items
        .iter()
        .map(|i| i.group.chars().count())
        .max()
        .unwrap_or(0);
    let nw = items
        .iter()
        .map(|i| i.name.chars().count())
        .max()
        .unwrap_or(0);
    for i in items {
        println!(
            "  {:<5}  {:<gw$}  {:<nw$}  {}",
            if i.kept { "keep" } else { "strip" },
            i.group,
            i.name,
            display_value(&i.value, full),
        );
    }
}

/// Makes a value safe to print on one line, shortening it unless `full`.
fn display_value(v: &str, full: bool) -> String {
    const MAX: usize = 70;
    let clean: String = v
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    if full || clean.chars().count() <= MAX {
        clean
    } else {
        let short: String = clean.chars().take(MAX - 1).collect();
        format!("{}\u{2026}", short)
    }
}

/// Writes to a temporary file next to `path`, then renames it over the original,
/// so an interrupted run never leaves a half-written image behind.
fn write_atomic(path: &Path, contents: &[u8], meta: &fs::Metadata) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{}.metarm-{}", name, process::id()));

    let result = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
        fs::set_permissions(&tmp, meta.permissions())?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}
