//! Standalone write benchmark; run only against an owned writable fixture copy.
//!
//! Compile this same source against both libraries. Only the candidate build
//! receives `--cfg prepared_upsert`; no substitute SQL is used in this driver.
//! Required arguments:
//!   --owned-root DIR --owned-db FILE --fresh-generation I64
//! Optional: --metadata-rows 32 --expected-rows 100000
//! DIR must contain a regular `.macevery-upsert-owned` file with exactly
//! `macevery-upsert-owned-v1\n`. Create that marker outside benchmark timing.
//!
//! Each phase times begin, prepare, writes, statement drop, and commit. Loading,
//! input mutations, full-field verification, hashing, file metadata, and JSON
//! encoding are outside phase timers. Process HWM includes the retained input
//! Vec and verification; it is not query memory or phase allocation evidence.
use macevery::{
    db::{ContentStamp, Database},
    model::FileRecord,
    MacEveryError,
};
use std::{
    env, fs,
    io::{self, Write},
    os::{
        raw::{c_int, c_long},
        unix::fs::MetadataExt,
    },
    path::{Path, PathBuf},
    time::Instant,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const OFFSET: u64 = 0xcbf29ce484222325;
const FIELDS: [&str; 13] = [
    "id",
    "path",
    "path_lower",
    "basename",
    "basename_lower",
    "ext_lower",
    "kind",
    "size",
    "mtime",
    "ctime",
    "dev",
    "inode",
    "indexed_at",
];

#[repr(C)]
#[derive(Default)]
struct Timeval {
    sec: c_long,
    #[cfg(target_os = "macos")]
    usec: c_int,
    #[cfg(target_os = "macos")]
    padding: c_int,
    #[cfg(not(target_os = "macos"))]
    usec: c_long,
}
#[repr(C)]
#[derive(Default)]
struct RUsage {
    user: Timeval,
    system: Timeval,
    maxrss: c_long,
    rest: [c_long; 13],
}
extern "C" {
    fn getrusage(who: c_int, usage: *mut RUsage) -> c_int;
}
fn usage() -> Result<RUsage> {
    let mut out = RUsage::default();
    if unsafe { getrusage(0, &mut out) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(out)
}
fn milliseconds(time: &Timeval) -> f64 {
    time.sec as f64 * 1000.0 + time.usec as f64 / 1000.0
}
fn peak_rss_bytes(usage: &RUsage) -> u64 {
    #[cfg(target_os = "macos")]
    {
        usage.maxrss.max(0) as u64
    }
    #[cfg(not(target_os = "macos"))]
    {
        usage.maxrss.max(0) as u64 * 1024
    }
}
fn json_string(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}
fn emit(json: String) -> Result<()> {
    let mut output = io::stdout().lock();
    writeln!(output, "{json}")?;
    output.flush()?;
    Ok(())
}
fn hash(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash = (*hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
    }
}

enum Field<'a> {
    Integer(i64),
    Text(&'a str),
    OptionalText(Option<&'a str>),
    OptionalInteger(Option<i64>),
}
struct Evidence {
    rows: usize,
    full: u64,
    fields: [u64; 13],
}
impl Evidence {
    fn new() -> Self {
        Self {
            rows: 0,
            full: OFFSET,
            fields: [OFFSET; 13],
        }
    }
    fn feed(&mut self, index: usize, bytes: &[u8]) {
        hash(&mut self.full, bytes);
        hash(&mut self.fields[index], bytes);
    }
    fn text(&mut self, index: usize, value: &str) {
        self.feed(index, &(value.len() as u64).to_le_bytes());
        self.feed(index, value.as_bytes());
    }
    fn record(&mut self, row: &FileRecord) {
        hash(&mut self.full, &[0xff]);
        let values = [
            Field::Integer(row.id),
            Field::Text(&row.path),
            Field::Text(&row.path_lower),
            Field::Text(&row.basename),
            Field::Text(&row.basename_lower),
            Field::OptionalText(row.ext_lower.as_deref()),
            Field::Text(row.kind.as_str()),
            Field::OptionalInteger(row.size),
            Field::OptionalInteger(row.mtime),
            Field::OptionalInteger(row.ctime),
            Field::OptionalInteger(row.dev),
            Field::OptionalInteger(row.inode),
            Field::Integer(row.indexed_at),
        ];
        for (index, field) in values.into_iter().enumerate() {
            self.feed(index, &[index as u8]);
            match field {
                Field::Integer(value) => {
                    self.feed(index, &[0]);
                    self.feed(index, &value.to_le_bytes());
                }
                Field::Text(value) => {
                    self.feed(index, &[1]);
                    self.text(index, value);
                }
                Field::OptionalText(None) => self.feed(index, &[2]),
                Field::OptionalText(Some(value)) => {
                    self.feed(index, &[3]);
                    self.text(index, value);
                }
                Field::OptionalInteger(None) => self.feed(index, &[4]),
                Field::OptionalInteger(Some(value)) => {
                    self.feed(index, &[5]);
                    self.feed(index, &value.to_le_bytes());
                }
            }
        }
        self.rows += 1;
    }
    fn json(&self) -> String {
        let mut full = self.full;
        hash(&mut full, &(self.rows as u64).to_le_bytes());
        let fields = FIELDS
            .iter()
            .zip(self.fields)
            .map(|(name, mut value)| {
                hash(&mut value, &(self.rows as u64).to_le_bytes());
                format!("{}:\"{value:016x}\"", json_string(name))
            })
            .collect::<Vec<_>>()
            .join(",");
        format!("{{\"algorithm\":\"typed-length-framed-fnv1a64-v1\",\"rows\":{},\"ordered_full_fnv1a64\":\"{full:016x}\",\"field_fnv1a64\":{{{fields}}}}}", self.rows)
    }
}

struct Arguments {
    root: PathBuf,
    database: PathBuf,
    generation: i64,
    metadata_rows: usize,
    expected_rows: Option<usize>,
}
fn arguments() -> Result<Arguments> {
    let mut root = None;
    let mut database = None;
    let mut generation = None;
    let mut metadata_rows = 32;
    let mut expected_rows = None;
    let mut args = env::args().skip(1);
    while let Some(key) = args.next() {
        if !matches!(
            key.as_str(),
            "--owned-root"
                | "--owned-db"
                | "--fresh-generation"
                | "--metadata-rows"
                | "--expected-rows"
        ) {
            return Err(format!("unknown argument {key}").into());
        }
        let value = args.next().ok_or("every flag requires a value")?;
        match key.as_str() {
            "--owned-root" => root = Some(PathBuf::from(value)),
            "--owned-db" => database = Some(PathBuf::from(value)),
            "--fresh-generation" => generation = Some(value.parse()?),
            "--metadata-rows" => metadata_rows = value.parse()?,
            "--expected-rows" => expected_rows = Some(value.parse()?),
            _ => unreachable!(),
        }
    }
    Ok(Arguments {
        root: root.ok_or("--owned-root is required")?,
        database: database.ok_or("--owned-db is required")?,
        generation: generation.ok_or("--fresh-generation is required")?,
        metadata_rows,
        expected_rows,
    })
}
fn check_owned_database(args: &Arguments) -> Result<PathBuf> {
    let root = fs::canonicalize(&args.root)?;
    let marker = root.join(".macevery-upsert-owned");
    let marker_metadata = fs::symlink_metadata(&marker)?;
    let marker_contents = b"macevery-upsert-owned-v1\n";
    if !marker_metadata.is_file()
        || marker_metadata.file_type().is_symlink()
        || marker_metadata.len() != marker_contents.len() as u64
        || fs::read(&marker)? != marker_contents
    {
        return Err("owned root requires the exact regular ownership marker".into());
    }
    let metadata = fs::symlink_metadata(&args.database)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.nlink() != 1 {
        return Err("owned database must be an existing regular non-hardlinked file".into());
    }
    let database = fs::canonicalize(&args.database)?;
    if !database.starts_with(&root) {
        return Err("database must reside under the owned fixture root".into());
    }
    if let Ok(live) = fs::canonicalize(macevery::config::default_db_path()) {
        if database == live {
            return Err("refusing the configured live database".into());
        }
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = database.as_os_str().to_os_string();
        sidecar.push(suffix);
        match fs::symlink_metadata(PathBuf::from(sidecar)) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("owned database sidecars must not be symlinks".into())
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(database)
}
fn database_sizes(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for (name, suffix) in [("main", ""), ("wal", "-wal"), ("shm", "-shm")] {
        let mut path = path.as_os_str().to_os_string();
        path.push(suffix);
        let value = match fs::metadata(PathBuf::from(path)) {
            Ok(metadata) => format!(
                "{{\"logical_bytes\":{},\"allocated_bytes\":{}}}",
                metadata.len(),
                metadata.blocks() * 512
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => "null".into(),
            Err(error) => return Err(error.into()),
        };
        parts.push(format!("{}:{value}", json_string(name)));
    }
    Ok(format!("{{{}}}", parts.join(",")))
}

fn write_records(db: &Database, rows: &[FileRecord]) -> macevery::Result<Option<usize>> {
    db.begin()?;
    let result = (|| {
        #[cfg(prepared_upsert)]
        let written = {
            let mut upsert = db.prepare_file_upsert()?;
            let mut written = 0;
            for row in rows {
                written += usize::from(upsert.upsert(row)?);
            }
            Some(written)
        };
        #[cfg(not(prepared_upsert))]
        let written = {
            for row in rows {
                db.upsert_file(row)?;
            }
            None
        };
        db.commit()?;
        Ok(written)
    })();
    if result.is_err() {
        let _ = db.rollback();
    }
    result
}
fn verify(db: &Database, expected: &[FileRecord]) -> macevery::Result<Evidence> {
    let mut evidence = Evidence::new();
    db.for_each_record(|actual| {
        if expected.get(evidence.rows) != Some(&actual) {
            return Err(MacEveryError::Sqlite(format!(
                "full FileRecord/order mismatch at ordinal {}",
                evidence.rows
            )));
        }
        evidence.record(&actual);
        Ok(())
    })?;
    if evidence.rows != expected.len() {
        return Err(MacEveryError::Sqlite("verified row count mismatch".into()));
    }
    Ok(evidence)
}

fn phase(
    db: &Database,
    database: &Path,
    rows: &[FileRecord],
    previous: &mut ContentStamp,
    name: &str,
    expected_public_field_changes: usize,
) -> Result<()> {
    let before = db.content_stamp()?;
    if before != *previous {
        return Err("fixture changed outside the preceding verified phase".into());
    }
    let before_sizes = database_sizes(database)?;
    emit(format!("{{\"event\":\"phase_start\",\"phase\":{},\"attempted_rows\":{},\"revision_before\":{},\"expected_public_field_changes\":{}}}", json_string(name), rows.len(), before.revision, expected_public_field_changes))?;
    let cpu_before = usage()?;
    let started = Instant::now();
    let result = write_records(db, rows);
    let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    let cpu_after = usage()?;
    let user_ms = milliseconds(&cpu_after.user) - milliseconds(&cpu_before.user);
    let system_ms = milliseconds(&cpu_after.system) - milliseconds(&cpu_before.system);
    let written = match result {
        Ok(written) => written,
        Err(error) => {
            emit(format!("{{\"event\":\"phase_error\",\"phase\":{},\"wall_ms\":{wall_ms:.6},\"cpu_ms\":{:.6},\"error\":{}}}", json_string(name), user_ms + system_ms, json_string(&error.to_string())))?;
            return Err(error.into());
        }
    };
    // Verification, hashing, metadata reads, and formatting begin after timers.
    db.begin_read()?;
    let verification = (|| {
        let after = db.content_stamp()?;
        let evidence = verify(db, rows)?;
        Ok::<_, MacEveryError>((after, evidence))
    })();
    let (after, evidence) = match verification {
        Ok(value) => {
            db.commit()?;
            value
        }
        Err(error) => {
            let _ = db.rollback();
            return Err(error.into());
        }
    };
    if after.database_id != before.database_id
        || after.file_identity != before.file_identity
        || after.revision < before.revision
    {
        return Err("database identity/revision changed unexpectedly".into());
    }
    let revision_delta = after.revision - before.revision;
    emit(format!(concat!("{{\"event\":\"phase_end\",\"phase\":{},\"rows_before\":{},\"rows_after\":{},\"attempted_rows\":{},",
        "\"expected_public_field_changes\":{},\"api_files_rows_written\":{},\"revision_before\":{},\"revision_after\":{},\"revision_delta\":{},",
        "\"revision_delta_equals_canonical_fixture_expectation\":{},\"wall_ms\":{:.6},\"cpu_ms\":{:.6},\"user_cpu_ms\":{:.6},\"system_cpu_ms\":{:.6},",
        "\"complete_ordered_field_equality\":true,\"fields_verified_per_record\":13,\"evidence\":{},\"database_files_before\":{},\"database_files_after\":{},",
        "\"process_cumulative_peak_rss_after_write_bytes\":{}}}"),
        json_string(name), rows.len(), evidence.rows, rows.len(), expected_public_field_changes,
        written.map_or_else(|| "null".into(), |n| n.to_string()), before.revision, after.revision, revision_delta,
        revision_delta as u64 == expected_public_field_changes as u64, wall_ms, user_ms + system_ms, user_ms, system_ms,
        evidence.json(), before_sizes, database_sizes(database)?, peak_rss_bytes(&cpu_after)))?;
    *previous = after;
    Ok(())
}

fn run() -> Result<()> {
    let args = arguments()?;
    let database = check_owned_database(&args)?;
    // Migration/opening and preload are intentionally not part of phase timing.
    let db = Database::open_existing(database.clone())?;
    db.begin_read()?;
    let mut rows = db.all_records()?;
    let mut stamp = db.content_stamp()?;
    db.commit()?;
    if rows.is_empty() || args.expected_rows.is_some_and(|count| count != rows.len()) {
        return Err("fixture must have the expected nonzero record count".into());
    }
    if rows.windows(2).any(|pair| pair[0].id >= pair[1].id) {
        return Err("fixture records must be in strictly increasing signed ID order".into());
    }
    if rows.iter().any(|row| row.indexed_at == args.generation) {
        return Err("fresh generation must differ from every initial indexed_at".into());
    }
    let mut evidence = Evidence::new();
    for row in &rows {
        evidence.record(row);
    }
    let string_capacity: usize = rows
        .iter()
        .map(|row| {
            row.path.capacity()
                + row.path_lower.capacity()
                + row.basename.capacity()
                + row.basename_lower.capacity()
                + row.ext_lower.as_ref().map_or(0, String::capacity)
        })
        .sum();
    let metadata_rows = args.metadata_rows.min(rows.len());
    if metadata_rows == 0 {
        return Err("--metadata-rows must be positive".into());
    }
    emit(format!(concat!("{{\"event\":\"start\",\"schema\":\"macevery.upsert-round8.v1\",\"pid\":{},\"caller_mode\":{},",
        "\"preloaded_rows\":{},\"fresh_generation\":{},\"sparse_metadata_rows\":{},\"initial_revision\":{},\"input_owned_capacity_bytes\":{},",
        "\"input_digest\":{},\"notes\":[",
        "\"Every phase replays the entire retained input Vec; sparse metadata changes only selected size values.\",",
        "\"Phase CPU/wall includes begin, production prepare/upsert, writer drop and commit; failure rollback is timed.\",",
        "\"Input loading/mutation, full field equality, hashes and JSON are outside phase timing.\",",
        "\"Canonical fixture revision expectations can differ on first replay if legacy stored lower/kind values normalize.\",",
        "\"Process HWM includes preload and preceding verification; it is not query memory or phase allocation.\",",
        "\"DB/WAL sizes and allocated blocks are snapshots, not physical bytes written; clones/checkpoints affect them.\",",
        "\"FNV1a64 is a stable noncryptographic digest; complete field equality is separately checked, not inferred from hashes.\",",
        "\"No private connection counters or total_changes production API are added.\"]}}"),
        std::process::id(), json_string(if cfg!(prepared_upsert) { "prepared_per_transaction" } else { "legacy_per_record" }),
        rows.len(), args.generation, metadata_rows, stamp.revision,
        rows.capacity() * std::mem::size_of::<FileRecord>() + string_capacity, evidence.json()))?;
    phase(&db, &database, &rows, &mut stamp, "identical_replay", 0)?;
    for row in &mut rows {
        row.indexed_at = args.generation;
    }
    phase(
        &db,
        &database,
        &rows,
        &mut stamp,
        "fresh_generation",
        rows.len(),
    )?;
    phase(
        &db,
        &database,
        &rows,
        &mut stamp,
        "identical_after_generation",
        0,
    )?;
    for selected in 0..metadata_rows {
        let index =
            (rows.len() / metadata_rows) * selected + (rows.len() % metadata_rows).min(selected);
        rows[index].size = Some(match rows[index].size {
            None => 0,
            Some(i64::MAX) => i64::MIN,
            Some(value) => value + 1,
        });
    }
    phase(
        &db,
        &database,
        &rows,
        &mut stamp,
        "sparse_metadata_replay",
        metadata_rows,
    )?;
    emit(format!("{{\"event\":\"complete\",\"phases\":4,\"ordered_all_fields_verified\":true,\"final_rows\":{},\"final_revision\":{},\"process_cumulative_peak_rss_bytes\":{}}}", rows.len(), stamp.revision, peak_rss_bytes(&usage()?)))?;
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        let _ = emit(format!(
            "{{\"event\":\"error\",\"error\":{}}}",
            json_string(&error.to_string())
        ));
        std::process::exit(1);
    }
}
