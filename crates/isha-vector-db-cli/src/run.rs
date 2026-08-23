//! Command dispatch.

use std::process::ExitCode;
use std::sync::Arc;

use isha_vector_db_core::api::{CompactOptions, Database, DatabaseConfig, VerifyLevel};
use isha_vector_db_core::clock::Clock;
use isha_vector_db_core::error::{ConfigError, Result};
use isha_vector_db_core::Include;
use isha_vector_db_storage_os::OsStorage;

use crate::format::{bytes, count, field, heading};

const USAGE: &str = "\
isha-vector-db — inspect, verify and compact a database

USAGE
  isha-vector-db <command> <path> [options]

COMMANDS
  stats     <path>              counts, sizes and configuration
  inspect   <path>              per-collection detail, including on-disk layout
  verify    <path> [--full]     integrity check; --full also cross-checks consistency
  compact   <path> [--min-dead R] [--all]
                                reclaim space from tombstoned rows
  get       <path> <collection> <id>
                                fetch one document
  version                       version and on-disk format information

OPTIONS
  --full          verify: read every byte and cross-check files against each other
  --quick         verify: headers and manifest only (the default is checksums)
  --all           compact: rewrite every segment, whatever its dead ratio
  --min-dead R    compact: rewrite segments at or above this dead ratio (default 0.3)
  --read-only     open without taking the write lock

Every command opens the database read-only unless it needs to write, so inspecting a database
an application is using is safe.
";

/// Wall-clock time.
///
/// The engine cannot read the clock itself — it performs no I/O of any kind — so a caller has to
/// supply one. This is that caller.
#[derive(Debug)]
struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Parsed command line.
struct Args<'a> {
    positional: Vec<&'a str>,
    flags: Vec<&'a str>,
    values: Vec<(&'a str, &'a str)>,
}

impl<'a> Args<'a> {
    fn parse(raw: &'a [String]) -> Self {
        let mut positional = Vec::new();
        let mut flags = Vec::new();
        let mut values = Vec::new();
        let mut i = 0;
        while i < raw.len() {
            let Some(arg) = raw.get(i).map(String::as_str) else {
                break;
            };
            if let Some(name) = arg.strip_prefix("--") {
                // A flag that takes a value consumes the next argument; everything else is a
                // boolean. Keeping that list explicit is simpler than a general grammar.
                if matches!(name, "min-dead") {
                    if let Some(v) = raw.get(i + 1) {
                        values.push((name, v.as_str()));
                        i += 2;
                        continue;
                    }
                }
                flags.push(name);
            } else {
                positional.push(arg);
            }
            i += 1;
        }
        Self {
            positional,
            flags,
            values,
        }
    }

    fn has(&self, flag: &str) -> bool {
        self.flags.contains(&flag)
    }

    fn value(&self, name: &str) -> Option<&'a str> {
        self.values
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| *v)
    }

    fn at(&self, index: usize) -> Option<&'a str> {
        self.positional.get(index).copied()
    }
}

/// Run a command.
pub(crate) fn dispatch(raw: &[String]) -> Result<ExitCode> {
    let args = Args::parse(raw);
    let Some(command) = args.at(0) else {
        print!("{USAGE}");
        return Ok(ExitCode::from(2));
    };

    match command {
        "version" => {
            println!("isha-vector-db {}", env!("CARGO_PKG_VERSION"));
            println!("on-disk format: v{}", isha_vector_db_format::FORMAT_VERSION);
            println!(
                "reads formats: v{}..=v{}",
                isha_vector_db_format::MIN_READABLE_VERSION,
                isha_vector_db_format::FORMAT_VERSION
            );
            Ok(ExitCode::SUCCESS)
        }
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        "stats" => stats(&args),
        "inspect" => inspect(&args),
        "verify" => verify(&args),
        "compact" => compact(&args),
        "get" => get(&args),
        other => {
            eprintln!("unknown command {other:?}\n");
            print!("{USAGE}");
            Ok(ExitCode::from(2))
        }
    }
}

fn require_path<'a>(args: &Args<'a>) -> Result<&'a str> {
    args.at(1).ok_or_else(|| {
        ConfigError::InvalidField {
            field: "path",
            value: "<missing>".to_owned(),
            constraint: "a database directory",
        }
        .into()
    })
}

/// Open a database. Read-only unless the command needs to write, so inspecting a database an
/// application is using cannot disturb it — or be blocked by it.
fn open(path: &str, writable: bool) -> Result<Database> {
    let storage = Arc::new(OsStorage::open(path)?);
    let config = if writable {
        DatabaseConfig::default().create_if_missing(false)
    } else {
        DatabaseConfig::default().read_only(true)
    };
    Database::open(storage, config, Arc::new(SystemClock))
}

fn stats(args: &Args<'_>) -> Result<ExitCode> {
    let path = require_path(args)?;
    let db = open(path, false)?;
    let s = db.stats()?;

    heading("Database");
    field("path", path);
    field("format version", s.format_version);
    field("manifest sequence", s.manifest_sequence);
    field("collections", s.collections);
    field("live documents", count(s.live_documents));
    field("rows on disk", count(s.total_rows));
    if s.total_rows > s.live_documents {
        let dead = s.total_rows - s.live_documents;
        field(
            "tombstoned rows",
            format!("{} (run `isha-vector-db compact`)", count(dead)),
        );
    }
    field("durable sync", s.durable_sync);

    for info in db.list_collections()? {
        let c = db.open_collection(&info.name)?;
        let cs = c.stats()?;
        heading(&format!("Collection: {}", cs.name));
        field("dimension", cs.dimension);
        field("metric", cs.metric.name());
        field("index", cs.index.name());
        field("live documents", count(cs.live_documents));
        field("rows on disk", count(cs.total_rows));
        field("segments", cs.segments);
        field("dead ratio", format!("{:.1}%", cs.dead_ratio * 100.0));
        if cs.buffered_documents > 0 {
            field(
                "buffered (unflushed)",
                format!(
                    "{} in {}",
                    count(cs.buffered_documents as u64),
                    bytes(cs.memtable_bytes as u64)
                ),
            );
        }
    }
    db.close()?;
    Ok(ExitCode::SUCCESS)
}

fn inspect(args: &Args<'_>) -> Result<ExitCode> {
    let path = require_path(args)?;
    let db = open(path, false)?;

    for info in db.list_collections()? {
        let c = db.open_collection(&info.name)?;
        let catalog = c.catalog();
        heading(&format!("Collection: {}", info.name));
        field("created (unix ms)", catalog.created_at_ms);
        field("dimension", catalog.dimension);
        field("metric", catalog.metric.name());
        field("id kind", format!("{:?}", catalog.id_kind));
        field("bytes per vector", catalog.row_stride());

        let stats = c.stats()?;
        field("segments", stats.segments);
        field(
            "vector data",
            bytes(stats.total_rows.saturating_mul(catalog.row_stride() as u64)),
        );
        // The amplification factor: what the database costs beyond the vectors themselves. The
        // number worth watching, because it is the one that surprises people on a phone.
        let raw = stats
            .live_documents
            .saturating_mul(catalog.row_stride() as u64);
        if raw > 0 {
            let stored = stats.total_rows.saturating_mul(catalog.row_stride() as u64);
            field(
                "storage amplification",
                format!("{:.2}x", stored as f64 / raw as f64),
            );
        }
    }
    db.close()?;
    Ok(ExitCode::SUCCESS)
}

fn verify(args: &Args<'_>) -> Result<ExitCode> {
    let path = require_path(args)?;
    let level = if args.has("full") {
        VerifyLevel::Full
    } else if args.has("quick") {
        VerifyLevel::Quick
    } else {
        VerifyLevel::Checksums
    };

    let db = open(path, false)?;
    let report = db.verify(level)?;

    heading(&format!("Verify ({level:?})"));
    field("collections", report.collections.len());
    field("segments checked", report.segments_checked());
    for c in &report.collections {
        field(
            &c.name,
            format!(
                "{} live, {} rows",
                count(c.live_documents),
                count(c.total_rows)
            ),
        );
    }

    if !report.warnings.is_empty() {
        heading("Warnings");
        for w in &report.warnings {
            println!("  ! {w}");
        }
    }
    if !report.errors.is_empty() {
        heading("Errors");
        for e in &report.errors {
            println!("  x {e}");
        }
    }

    db.close()?;
    if report.is_clean() {
        println!("\nok — no problems found");
        Ok(ExitCode::SUCCESS)
    } else {
        // A distinct exit code, so a script can tell "damaged" from "could not run".
        println!("\n{} problem(s) found", report.errors.len());
        Ok(ExitCode::from(3))
    }
}

fn compact(args: &Args<'_>) -> Result<ExitCode> {
    let path = require_path(args)?;
    let mut options = CompactOptions::default();
    if args.has("all") {
        options = CompactOptions::everything();
    }
    if let Some(raw) = args.value("min-dead") {
        let ratio: f32 = raw.parse().map_err(|_| -> isha_vector_db_core::DbError {
            ConfigError::InvalidField {
                field: "--min-dead",
                value: raw.to_owned(),
                constraint: "a number between 0 and 1",
            }
            .into()
        })?;
        if !(0.0..=1.0).contains(&ratio) {
            return Err(ConfigError::InvalidField {
                field: "--min-dead",
                value: raw.to_owned(),
                constraint: "between 0 and 1",
            }
            .into());
        }
        options = options.min_dead_ratio(ratio);
    }

    let db = open(path, true)?;
    let before = db.stats()?;
    let report = db.compact(options)?;
    let after = db.stats()?;

    heading("Compact");
    field("segments rewritten", report.segments_rewritten);
    field("segments created", report.segments_created);
    field("rows reclaimed", count(report.rows_reclaimed));
    field(
        "rows on disk",
        format!("{} → {}", count(before.total_rows), count(after.total_rows)),
    );
    if report.segments_rewritten == 0 {
        println!("\nnothing to do — no segment was dead enough to be worth rewriting");
        println!("use --all to rewrite every segment regardless");
    }
    db.close()?;
    Ok(ExitCode::SUCCESS)
}

fn get(args: &Args<'_>) -> Result<ExitCode> {
    let path = require_path(args)?;
    let (Some(collection), Some(id)) = (args.at(2), args.at(3)) else {
        return Err(ConfigError::InvalidField {
            field: "arguments",
            value: "<missing>".to_owned(),
            constraint: "isha-vector-db get <path> <collection> <id>",
        }
        .into());
    };

    let db = open(path, false)?;
    let c = db.open_collection(collection)?;
    let doc_id = match c.catalog().id_kind {
        isha_vector_db_format::IdKind::U64 => match id.parse::<u64>() {
            Ok(v) => isha_vector_db_core::DocId::U64(v),
            Err(_) => {
                return Err(ConfigError::InvalidField {
                    field: "id",
                    value: id.to_owned(),
                    constraint: "an integer, since this collection uses u64 ids",
                }
                .into())
            }
        },
        _ => isha_vector_db_core::DocId::from(id),
    };

    match c.get_with(&doc_id, Include::ALL)? {
        None => {
            println!("not found");
            db.close()?;
            return Ok(ExitCode::from(4));
        }
        Some(doc) => {
            heading(&format!("Document: {}", doc.id.display()));
            if let Some(v) = &doc.vector {
                let preview: Vec<String> = v.iter().take(8).map(|x| format!("{x:.4}")).collect();
                let ellipsis = if v.len() > 8 { ", …" } else { "" };
                field(
                    "vector",
                    format!("[{}{ellipsis}] ({} dims)", preview.join(", "), v.len()),
                );
            }
            if doc.metadata.is_empty() {
                field("metadata", "(none)");
            } else {
                field("metadata", "");
                for (k, value) in doc.metadata.iter() {
                    println!("    {k} = {value:?}");
                }
            }
            match &doc.content {
                Some(c) => field("content", format!("{} bytes", count(c.len() as u64))),
                None => field("content", "(none)"),
            }
        }
    }
    db.close()?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn flags_and_positionals_are_separated() {
        let raw = args(&["compact", "/tmp/db", "--all", "--min-dead", "0.5"]);
        let a = Args::parse(&raw);
        assert_eq!(a.at(0), Some("compact"));
        assert_eq!(a.at(1), Some("/tmp/db"));
        assert!(a.has("all"));
        assert_eq!(a.value("min-dead"), Some("0.5"));
        assert!(!a.has("full"));
    }

    #[test]
    fn a_value_flag_at_the_end_without_its_value_is_treated_as_a_flag() {
        let raw = args(&["compact", "/tmp/db", "--min-dead"]);
        let a = Args::parse(&raw);
        assert_eq!(a.value("min-dead"), None);
        assert!(a.has("min-dead"));
    }

    #[test]
    fn no_arguments_prints_usage_and_exits_nonzero() {
        assert_eq!(dispatch(&[]).unwrap(), ExitCode::from(2));
    }

    #[test]
    fn an_unknown_command_exits_nonzero() {
        assert_eq!(dispatch(&args(&["frobnicate"])).unwrap(), ExitCode::from(2));
    }

    #[test]
    fn version_and_help_succeed() {
        assert_eq!(dispatch(&args(&["version"])).unwrap(), ExitCode::SUCCESS);
        assert_eq!(dispatch(&args(&["help"])).unwrap(), ExitCode::SUCCESS);
    }

    #[test]
    fn a_missing_path_is_an_argument_error() {
        assert!(dispatch(&args(&["stats"])).is_err());
    }

    #[test]
    fn the_clock_returns_a_plausible_time() {
        // Sanity, not precision: anything after 2020 means the epoch arithmetic is right.
        assert!(SystemClock.now_ms() > 1_577_836_800_000);
    }
}
