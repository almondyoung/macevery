//! Isolated round-9 mechanism driver, compiled against each frozen library.
//! Only the controller's marked, fresh clone is writable. No App, real file
//! watcher, memory pressure, allocator purge, or production diagnostics changes.
use macevery::{
    compact_index::{ApplyOutcome, CompactIndex},
    db::{ContentStamp, Database, FileChange, FileChangeRead},
    model::{FileKind, FileRecord, SearchOptions, SearchResult},
    search,
    sqlite::{Connection, Step},
};
use std::{
    env,
    error::Error,
    fs,
    io::{self, Write},
    os::raw::{c_int, c_long},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const PROBE_EXT: &str = "round9probe";
const ROOT: &str = "/__MacEvery_Memory_Round9__";
const TEXT_BYTES: usize = 256 * 1024;
const WAVE_ROWS: usize = 8;

#[repr(C)]
#[derive(Default)]
struct Timeval {
    sec: c_long,
    usec: c_int,
    padding: c_int,
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
    fn getrusage(who: c_int, out: *mut RUsage) -> c_int;
    // These adapters are compiled by clang from this host's SDK headers.
    fn round9_stats_json(out: *mut u8, capacity: usize) -> c_int;
    fn round9_sha256(data: *const u8, len: usize, digest: *mut u8) -> c_int;
}
fn cpu_us() -> i64 {
    let mut out = RUsage::default();
    assert_eq!(unsafe { getrusage(0, &mut out) }, 0);
    (out.user.sec + out.system.sec) * 1_000_000
        + i64::from(out.user.usec)
        + i64::from(out.system.usec)
}
fn quoted(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
fn emit(value: String) -> Result<()> {
    if value.len() > 32 * 1024 {
        return Err("bounded JSONL event exceeded 32KiB".into());
    }
    println!("{value}");
    io::stdout().flush()?;
    Ok(())
}
fn gate() -> Result<()> {
    let mut line = String::new();
    if io::stdin().read_line(&mut line)? == 0 || line.trim() != "go" {
        return Err("controller gate closed".into());
    }
    Ok(())
}
fn os_stats() -> String {
    let mut out = [0u8; 4096];
    let len = unsafe { round9_stats_json(out.as_mut_ptr(), out.len()) };
    if len <= 0 || len as usize >= out.len() {
        return "{\"adapter_error\":true}".into();
    }
    String::from_utf8_lossy(&out[..len as usize]).into_owned()
}
fn accounting(index: &CompactIndex) -> String {
    let m = index.memory_breakdown();
    let f = index.fragmentation_stats();
    format!(
        concat!(
            "{{\"records\":{},\"packed_capacity_bytes\":{},",
            "\"record_capacity_bytes\":{},\"metadata_capacity_bytes\":{},",
            "\"string_capacity_bytes\":{},\"string_used_bytes\":{},",
            "\"string_unused_capacity_bytes\":{},\"extension_capacity_bytes\":{},",
            "\"container_bytes\":{},\"prefix_string_bytes\":{},",
            "\"prefix_entry_capacity_bytes\":{},\"prefix_lookup_capacity_bytes\":{},",
            "\"prefix_entries\":{},\"narrow_metadata_blocks\":{},",
            "\"wide_metadata_blocks\":{},\"live_metadata_escapes\":{},",
            "\"dead_records\":{},\"dead_string_bytes\":{},\"extension_growth_bytes\":{}}}"
        ),
        index.len(),
        index.memory_usage_bytes(),
        m.record_capacity_bytes,
        m.metadata_capacity_bytes,
        m.string_capacity_bytes,
        m.string_used_bytes,
        m.string_capacity_bytes.saturating_sub(m.string_used_bytes),
        m.extension_capacity_bytes,
        m.container_bytes,
        m.prefix_string_bytes,
        m.prefix_entry_capacity_bytes,
        m.prefix_lookup_capacity_bytes,
        m.prefix_entries,
        m.narrow_metadata_blocks,
        m.wide_metadata_blocks,
        m.live_metadata_escapes,
        f.dead_records,
        f.dead_string_bytes,
        f.extension_growth_bytes
    )
}
fn point(phase: &str, index: Option<&CompactIndex>) -> Result<()> {
    // Obtain malloc/VM accounting before formatting the allocation accounting.
    let os = os_stats();
    let packed = index.map(accounting).unwrap_or_else(|| "null".into());
    emit(format!(
        "{{\"event\":\"memory\",\"phase\":{},\"packed\":{},\"os\":{}}}",
        quoted(phase),
        packed,
        os
    ))
}
fn phase<T>(name: &str, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    emit(format!(
        "{{\"event\":\"phase_begin\",\"phase\":{}}}",
        quoted(name)
    ))?;
    gate()?;
    let before = cpu_us();
    let start = Instant::now();
    let value = operation()?;
    let wall = start.elapsed().as_secs_f64();
    let cpu = cpu_us() - before;
    emit(format!(
        "{{\"event\":\"phase_end\",\"phase\":{},\"wall_seconds\":{:.9},\"cpu_us\":{}}}",
        quoted(name),
        wall,
        cpu
    ))?;
    Ok(value)
}
fn put_text(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as u64).to_le_bytes());
    out.extend_from_slice(text.as_bytes());
}
fn put_optional_i64(out: &mut Vec<u8>, value: Option<i64>) {
    out.push(u8::from(value.is_some()));
    if let Some(v) = value {
        out.extend_from_slice(&v.to_le_bytes());
    }
}
fn result_hash(rows: &[SearchResult]) -> Result<String> {
    // Versioned, length-prefixed UTF-8 and LE integer encoding of all 13 fields,
    // including NULL tags, plus score and result order; no Debug-format digest.
    let mut data = b"macevery-round9-ordered-13-fields-plus-score-v1\0".to_vec();
    data.extend_from_slice(&(rows.len() as u64).to_le_bytes());
    for result in rows {
        let r = &result.record;
        data.extend_from_slice(&r.id.to_le_bytes());
        for text in [&r.path, &r.path_lower, &r.basename, &r.basename_lower] {
            put_text(&mut data, text);
        }
        data.push(u8::from(r.ext_lower.is_some()));
        if let Some(ext) = &r.ext_lower {
            put_text(&mut data, ext);
        }
        put_text(&mut data, r.kind.as_str());
        for value in [r.size, r.mtime, r.ctime, r.dev, r.inode] {
            put_optional_i64(&mut data, value);
        }
        data.extend_from_slice(&r.indexed_at.to_le_bytes());
        data.extend_from_slice(&result.score.to_le_bytes());
        if data.len() > 16 * 1024 * 1024 {
            return Err("ordered result digest input exceeded 16MiB".into());
        }
    }
    let mut digest = [0u8; 32];
    if unsafe { round9_sha256(data.as_ptr(), data.len(), digest.as_mut_ptr()) } != 0 {
        return Err("SHA256 SDK adapter failed".into());
    }
    Ok(digest.iter().map(|v| format!("{v:02x}")).collect())
}
fn verify(phase_name: &str, index: &CompactIndex, db: &Database, queries: &[String]) -> Result<()> {
    emit(format!(
        "{{\"event\":\"verification_begin\",\"phase\":{}}}",
        quoted(phase_name)
    ))?;
    for query in queries {
        let options = SearchOptions {
            query: query.clone(),
            limit: 300,
            ..SearchOptions::default()
        };
        let before = cpu_us();
        let timer = Instant::now();
        let actual = index.search(&options);
        let wall = timer.elapsed().as_secs_f64();
        let cpu = cpu_us() - before;
        // Independent SQLite/search path. Its work and hashing are excluded
        // from measured packed-query CPU/wall, but remain in process lifetime.
        let expected = search::search(db, &options)?;
        if actual != expected {
            return Err(format!(
                "full ordered 13-field result/score mismatch: {phase_name}/{query}"
            )
            .into());
        }
        emit(format!(
            concat!(
                "{{\"event\":\"query_verified\",\"phase\":{},",
                "\"query\":{},\"count\":{},\"sha256\":{},",
                "\"all_13_fields_and_score_equal\":true,",
                "\"packed_query_wall_seconds\":{:.9},\"packed_query_cpu_us\":{}}}"
            ),
            quoted(phase_name),
            quoted(query),
            actual.len(),
            quoted(&result_hash(&actual)?),
            wall,
            cpu
        ))?;
    }
    point(&format!("{phase_name}_after_validation"), Some(index))?;
    emit(format!(
        "{{\"event\":\"verification_end\",\"phase\":{}}}",
        quoted(phase_name)
    ))
}
fn changes(db: &Database, stamp: &ContentStamp) -> Result<Vec<FileChange>> {
    db.begin_read()?;
    let result = db.file_changes_since(stamp, 4096, 4096);
    db.commit()?;
    match result? {
        FileChangeRead::Available { changes, .. } => Ok(changes),
        FileChangeRead::Fallback { reason } => {
            Err(format!("unexpected journal fallback in bounded harness: {reason}").into())
        }
    }
}
fn upserts(db: &Database, rows: &[FileRecord]) -> Result<Vec<FileChange>> {
    let stamp = db.content_stamp()?;
    db.begin()?;
    let result = (|| -> Result<()> {
        let mut prepared = db.prepare_file_upsert()?;
        for row in rows {
            prepared.upsert(row)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        db.rollback()?;
        return Err(error);
    }
    db.commit()?;
    changes(db, &stamp)
}
fn replace_text(
    db: &Database,
    writer: &Connection,
    rows: &[FileRecord],
) -> Result<Vec<FileChange>> {
    let stamp = db.content_stamp()?;
    writer.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<()> {
        let mut statement = writer.prepare("UPDATE files SET path=?1,path_lower=?2,basename=?3,basename_lower=?4,ext_lower=?5 WHERE id=?6")?;
        for row in rows {
            statement.reset()?;
            statement.bind_text(1, Some(&row.path))?;
            statement.bind_text(2, Some(&row.path_lower))?;
            statement.bind_text(3, Some(&row.basename))?;
            statement.bind_text(4, Some(&row.basename_lower))?;
            statement.bind_text(5, row.ext_lower.as_deref())?;
            statement.bind_i64(6, Some(row.id))?;
            if !matches!(statement.step()?, Step::Done) {
                return Err("UPDATE returned row".into());
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        writer.execute_batch("ROLLBACK")?;
        return Err(error);
    }
    writer.execute_batch("COMMIT")?;
    changes(db, &stamp)
}
fn apply(
    index: &mut Option<CompactIndex>,
    db: &Database,
    delta: &[FileChange],
    name: &str,
) -> Result<bool> {
    let outcome = phase(name, || Ok(index.as_mut().unwrap().apply_changes(delta)?))?;
    point(name, index.as_ref())?;
    match outcome {
        ApplyOutcome::Applied => {
            emit(format!("{{\"event\":\"delta_outcome\",\"phase\":{},\"changes\":{},\"outcome\":\"applied\"}}", quoted(name), delta.len()))?;
            Ok(false)
        }
        ApplyOutcome::NeedsFullReload(reason) => {
            emit(format!("{{\"event\":\"delta_outcome\",\"phase\":{},\"changes\":{},\"outcome\":\"needs_full_reload\",\"reason\":{}}}", quoted(name), delta.len(), quoted(reason)))?;
            phase(&format!("{name}_drop_then_full_reload"), || {
                drop(index.take());
                *index = Some(CompactIndex::load(db)?);
                Ok(())
            })?;
            point(&format!("{name}_reloaded"), index.as_ref())?;
            Ok(true)
        }
    }
}
fn rewritten(row: &FileRecord, wave: usize, bytes: usize) -> FileRecord {
    let suffix = format!("-code-{wave:04}-{:08}.{PROBE_EXT}", row.id);
    let path = format!(
        "{ROOT}/churn/{}{}",
        "z".repeat(bytes.saturating_sub(suffix.len() + ROOT.len() + 8)),
        suffix
    );
    let mut result = FileRecord::new(path, row.kind.clone(), row.indexed_at);
    result.id = row.id;
    result.size = row.size;
    result.mtime = row.mtime;
    result.ctime = row.ctime;
    result.dev = row.dev;
    result.inode = row.inode;
    result
}
fn main() -> Result<()> {
    if env::consts::OS != "macos" {
        return Err("macOS only".into());
    }
    let args: Vec<_> = env::args().collect();
    let arg = |key: &str| -> Result<String> {
        args.windows(2)
            .find(|v| v[0] == key)
            .map(|v| v[1].clone())
            .ok_or_else(|| format!("missing {key}").into())
    };
    let root = PathBuf::from(arg("--owned-root")?).canonicalize()?;
    let path = PathBuf::from(arg("--owned-db")?).canonicalize()?;
    if !root.starts_with("/private/tmp")
        || path != root.join("index.sqlite")
        || fs::symlink_metadata(root.join("index.sqlite"))?
            .file_type()
            .is_symlink()
        || fs::read_to_string(root.join(".macevery-memory-round9-owned"))?
            != "macevery-memory-round9-v1\n"
    {
        return Err("unmarked/non-isolated writable DB refused".into());
    }
    let idle_ms: u64 = arg("--idle-ms")?.parse()?;
    let reserve_bytes: usize = arg("--reserve-bytes")?.parse()?;
    let churn_bytes: usize = arg("--churn-bytes")?.parse()?;
    if idle_ms > 10_000 || reserve_bytes > 70 * 1024 * 1024 || churn_bytes > 40 * 1024 * 1024 {
        return Err("experiment bounds exceeded".into());
    }
    let queries: Vec<_> = arg("--queries")?.split(',').map(str::to_string).collect();
    if queries.is_empty() || queries.len() > 12 || queries.iter().any(|q| q.len() > 128) {
        return Err("query set outside bounds".into());
    }
    emit("{\"event\":\"start\",\"schema\":1,\"owned_clone_only\":true,\"synthetic_long_text_is_not_native_path_workload\":true}".into())?;
    let db = Database::open_existing(path.clone())?;
    let writer = Connection::open_existing(Path::new(&path))?;
    let mut index = Some(phase("load", || Ok(CompactIndex::load(&db)?))?);
    point("load_before_validation", index.as_ref())?;
    phase("loaded_idle", || {
        thread::sleep(Duration::from_millis(idle_ms));
        Ok(())
    })?;
    point("loaded_idle", index.as_ref())?;
    verify("initial", index.as_ref().unwrap(), &db, &queries)?;
    let mut probes = db.filtered_records(
        &SearchOptions {
            ext: Some(PROBE_EXT.into()),
            ..SearchOptions::default()
        },
        4096,
    )?;
    if probes.len() < 1025
        || probes.len() > 2048
        || probes.iter().any(|r| !r.path.starts_with(ROOT))
    {
        return Err("prepared mother must contain 1025..2048 owned probe rows only".into());
    }
    probes.sort_by_key(|r| r.id);
    for row in &mut probes {
        row.size = Some(row.size.unwrap_or(0) + 1);
        row.indexed_at += 1;
    }
    let delta = upserts(&db, &probes)?;
    apply(&mut index, &db, &delta, "metadata_delta")?;
    drop(delta);
    point("metadata_delta_payload_dropped", index.as_ref())?;
    verify("metadata_delta", index.as_ref().unwrap(), &db, &queries)?;

    let renamed: Vec<_> = probes[..WAVE_ROWS]
        .iter()
        .map(|r| rewritten(r, 0, 1024))
        .collect();
    let delta = replace_text(&db, &writer, &renamed)?;
    apply(&mut index, &db, &delta, "rename_text_delta")?;
    probes[..WAVE_ROWS].clone_from_slice(&renamed);
    drop(delta);
    drop(renamed);
    verify("rename_text_delta", index.as_ref().unwrap(), &db, &queries)?;

    let stamp = db.content_stamp()?;
    db.begin()?;
    for row in &probes[WAVE_ROWS..] {
        db.delete_path_exact(&row.path)?;
    }
    db.commit()?;
    let delta = changes(&db, &stamp)?;
    apply(&mut index, &db, &delta, "deletion_delta")?;
    drop(delta);
    probes.truncate(WAVE_ROWS);
    verify("deletion_delta", index.as_ref().unwrap(), &db, &queries)?;

    let mut fallbacks = 0;
    let waves = churn_bytes.div_ceil(WAVE_ROWS * TEXT_BYTES);
    for wave in 0..waves {
        let rewritten: Vec<_> = probes
            .iter()
            .map(|r| rewritten(r, wave + 1, TEXT_BYTES))
            .collect();
        let delta = replace_text(&db, &writer, &rewritten)?;
        fallbacks += usize::from(apply(
            &mut index,
            &db,
            &delta,
            &format!("fragmentation_{wave:02}"),
        )?);
        probes = rewritten;
        drop(delta);
        point(
            &format!("fragmentation_{wave:02}_payload_dropped"),
            index.as_ref(),
        )?;
    }
    emit(format!("{{\"event\":\"fragmentation_summary\",\"requested_text_bytes\":{},\"waves\":{},\"full_reloads\":{}}}", churn_bytes, waves, fallbacks))?;
    verify(
        "fragmentation_complete",
        index.as_ref().unwrap(),
        &db,
        &queries,
    )?;

    let before_reserve = index
        .as_ref()
        .unwrap()
        .memory_breakdown()
        .string_capacity_bytes;
    let reserve_waves = reserve_bytes.div_ceil(WAVE_ROWS * TEXT_BYTES);
    for wave in 0..reserve_waves {
        let rows: Vec<_> = (0..WAVE_ROWS)
            .map(|row| {
                let suffix = format!("-{wave:03}-{row:03}-code.round9reserve");
                FileRecord::new(
                    format!(
                        "{ROOT}/reserve/{}{}",
                        "q".repeat(TEXT_BYTES - suffix.len()),
                        suffix
                    ),
                    FileKind::File,
                    123,
                )
            })
            .collect();
        let delta = upserts(&db, &rows)?;
        apply(&mut index, &db, &delta, &format!("reserve_{wave:02}"))?;
        drop(delta);
        drop(rows);
        point(
            &format!("reserve_{wave:02}_payload_dropped"),
            index.as_ref(),
        )?;
    }
    emit(format!("{{\"event\":\"reserve_summary\",\"requested_text_bytes\":{},\"waves\":{},\"capacity_before\":{},\"capacity_after\":{}}}", reserve_bytes, reserve_waves, before_reserve, index.as_ref().unwrap().memory_breakdown().string_capacity_bytes))?;
    verify("reserve_complete", index.as_ref().unwrap(), &db, &queries)?;
    drop(probes);
    phase("final_idle", || {
        thread::sleep(Duration::from_millis(idle_ms));
        Ok(())
    })?;
    point("final_idle", index.as_ref())?;
    phase("drop_index", || {
        drop(index.take());
        Ok(())
    })?;
    point("index_dropped", None)?;
    drop(writer);
    drop(db);
    phase("closed_idle", || {
        thread::sleep(Duration::from_millis(idle_ms));
        Ok(())
    })?;
    point("db_and_index_dropped", None)?;
    emit("{\"event\":\"complete\",\"status\":\"PASS\",\"oracle\":\"independent_SQLite_search_full_13_fields_plus_score_order\"}".into())
}
