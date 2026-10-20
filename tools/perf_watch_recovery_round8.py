#!/usr/bin/env python3
"""Small-tree watch recovery A/B/A using each version's actual private decisions.

Run only when the coordinating thread has scheduled exclusive measurement:
  python3 tools/perf_watch_recovery_round8.py \
    --baseline-source .build/watch-recovery-20261020/baseline/source \
    --baseline-library .build/watch-recovery-20261020/baseline/library/libmacevery.rlib \
    --candidate-source . --candidate-library target/release/libmacevery.rlib \
    --output /private/tmp/macevery-watch-round8-UNIQUE

No Cargo build or production source edit occurs. rustc builds private copies of
watch.rs/scanner.rs with a tool-only entry inside macos_impl, linking the supplied rlibs.
The caller must prove each rlib matches its frozen source. This is deterministic
flag injection through production EventFilter/add/process/checkpoint functions,
not a real FSEvents delivery test. Each operation scans unchanged real files;
new indexed_at values must still write/journal under the public contract.

The fixture, mother DB, logs and every fresh APFS clone stay below the new /tmp
output directory. The mother DB is hash-pinned; no live DB/App/listener is used.
Default: two roots with 3,000 files each, 100 in the event subtree. Must-only,
trueDropped, ordinary each use baseline/candidate/baseline. Separate candidate
off/on/off groups isolate diagnostic logging cost. Preparation/copy/validation
are outside scan timing. Scan CPU is getrusage user+system (all threads); 20ms
libproc RSS/footprint are sampled lower bounds. wait4 CPU/HWM includes the whole
owned process and correctness validation and must not be called scan-only cost.
Neither APFS clones nor fresh processes imply cold filesystem caches.
"""
import argparse
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import time

import check_service_round2 as shared


def write_json(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n')


COMMON_ENTRY = r'''
    #[derive(Default)]
    struct Round8Stats { indexed: u64, written: u64, unchanged: u64, deleted: u64, warnings: usize }
    fn round8_accumulate(stats: &std::cell::RefCell<Round8Stats>, summary: &IndexSummary) {
        let mut stats = stats.borrow_mut();
        stats.indexed += summary.indexed;
        stats.written += __WRITTEN__;
        stats.unchanged += __UNCHANGED__;
        stats.deleted += summary.deleted;
        stats.warnings += summary.warnings.len();
    }
    #[repr(C)]
    #[derive(Default)]
    struct Round8Timeval { sec: i64, usec: i32, padding: i32 }
    #[repr(C)]
    #[derive(Default)]
    struct Round8Rusage { user: Round8Timeval, system: Round8Timeval, maxrss: i64, rest: [i64; 13] }
    extern "C" { fn getrusage(who: i32, out: *mut Round8Rusage) -> i32; }
    fn round8_cpu_us() -> i64 {
        let mut usage = Round8Rusage::default();
        assert_eq!(unsafe { getrusage(0, &mut usage) }, 0);
        (usage.user.sec + usage.system.sec) * 1_000_000 + i64::from(usage.user.usec) + i64::from(usage.system.usec)
    }
    fn round8_gate() {
        let mut line = String::new();
        assert!(std::io::stdin().read_line(&mut line).unwrap() > 0, "controller closed gate");
        assert_eq!(line.trim(), "go");
    }
    fn round8_print(line: &str) {
        use std::io::Write;
        println!("{line}");
        std::io::stdout().flush().unwrap();
    }
    pub fn round8_entry(args: &[String]) -> crate::error::Result<()> {
        let db_path = PathBuf::from(&args[1]);
        let roots = [PathBuf::from(&args[2]), PathBuf::from(&args[3])];
        let db = Database::open(db_path.clone())?;
        if args[0] == "seed" {
            crate::scanner::index_paths(&db, &crate::scanner::IndexRequest {
                roots: roots.to_vec(), excludes: vec![".git".into()], rebuild: true,
            })?;
            db.set_meta(META_LAST_EVENT_ID, "100")?;
            round8_print("{\"phase\":\"seeded\"}");
            return Ok(());
        }
        assert_eq!(args[0], "run");
        let scenario = args[4].as_str();
        let diagnostics = args[5] == "on";
        let before = db.all_records()?;
        let stamp_before = db.content_stamp()?;
        let checkpoint_before = db.get_meta(META_LAST_EVENT_ID)?.unwrap_or_default();
        assert_eq!(checkpoint_before, "100");
        let path = roots[0].join("target");
        let flags = match scenario {
            "must" => FLAG_MUST_SCAN_SUBDIRS,
            "dropped" => FLAG_MUST_SCAN_SUBDIRS | FLAG_USER_DROPPED,
            "ordinary" => 0,
            _ => panic!("unknown scenario"),
        };
        let excludes = runtime_excludes(vec![".git".into()], &db_path);
        let filter = EventFilter::new(&db_path, &excludes);
        let stats = std::cell::RefCell::new(Round8Stats::default());
        let mut pending = PendingWork::default();
        __LOGGER__
        round8_print("{\"phase\":\"ready\"}");
        round8_gate();
        let cpu_before = round8_cpu_us();
        let started = std::time::Instant::now();
        let event = FsEvent { path: path.to_string_lossy().into_owned(), flags, id: 101 };
        assert!(filter.accepts(&event.path, event.flags));
        __ADD__
        __FILTER_LOG__
        __PROCESS__(
            &mut pending, true,
            |path| { let summary = refresh_path(&db, path, &excludes)?; round8_accumulate(&stats, &summary); Ok(summary) },
            || { let summary = refresh_roots(&db, &roots, &excludes)?; round8_accumulate(&stats, &summary); Ok(summary) }
            __LOG_ARG__
        );
        let checkpoint_complete = __CHECKPOINT__;
        let cpu_us = round8_cpu_us() - cpu_before;
        let wall_us = started.elapsed().as_micros();
        round8_print(&format!("{{\"phase\":\"measured\",\"scan_wall_us\":{wall_us},\"scan_cpu_us\":{cpu_us}}}"));
        round8_gate();
        let after = db.all_records()?;
        let stamp_after = db.content_stamp()?;
        let equal_without_generation = before.len() == after.len() && before.iter().zip(&after).all(|(old, new)| {
            let mut old = old.clone(); old.indexed_at = new.indexed_at; old == *new
        });
        let neighbor_before = before.iter().filter(|row| Path::new(&row.path).starts_with(&roots[1])).collect::<Vec<_>>();
        let neighbor_after = after.iter().filter(|row| Path::new(&row.path).starts_with(&roots[1])).collect::<Vec<_>>();
        let neighbor_unchanged = neighbor_before == neighbor_after;
        let changed_indexed_at = before.iter().zip(&after).filter(|(old, new)| old.indexed_at != new.indexed_at).count();
        let stats = stats.borrow();
        let checkpoint = db.get_meta(META_LAST_EVENT_ID)?.unwrap_or_default();
        assert!(checkpoint_complete && !pending.has_pending(), "work not complete");
        assert_eq!(checkpoint, "101");
        assert_eq!(stats.warnings, 0);
        assert!(equal_without_generation, "complete FileRecord/ID changed beyond generation");
        round8_print(&format!(concat!(
            "{{\"phase\":\"validated\",\"records\":{},\"visited\":{},\"rows_written\":{},\"unchanged\":{},",
            "\"stale_removed\":{},\"warnings\":{},\"revision_before\":{},\"revision_after\":{},",
            "\"checkpoint_before\":{},\"checkpoint\":{},\"all_fields_and_ids_equal_except_indexed_at\":{},\"neighbor_full_records_unchanged\":{},\"indexed_at_changed_rows\":{}}}"),
            after.len(), stats.indexed, stats.written, stats.unchanged, stats.deleted, stats.warnings,
            stamp_before.revision, stamp_after.revision, checkpoint_before, checkpoint, equal_without_generation, neighbor_unchanged, changed_indexed_at));
        Ok(())
    }
'''


def build_harness(source_root, library, output, candidate):
    source = source_root / 'src/watch.rs'
    text = source.read_text()
    shared.require(('fn add_scoped(' in text) == candidate, 'Source role/API does not match old/new watch contract')
    entry = COMMON_ENTRY.replace('__WRITTEN__', 'summary.rows_written' if candidate else 'summary.indexed')
    entry = entry.replace('__UNCHANGED__', 'summary.unchanged' if candidate else '0')
    entry = entry.replace('__LOGGER__', 'let mut logger = TriggerLog::new(&db_path);' if candidate else 'assert!(!diagnostics);')
    entry = entry.replace('__ADD__', 'pending.add_scoped(event, &roots);' if candidate else 'pending.add(event);')
    entry = entry.replace('__FILTER_LOG__', '''if diagnostics {
            logger.record("filter_summary", "callback_interval", &pending, None, LogInfo {
                received: filter.counters.received.swap(0, Ordering::Relaxed),
                rejected: filter.counters.rejected.swap(0, Ordering::Relaxed),
                received_flags: filter.counters.flags.swap(0, Ordering::Relaxed),
                ..LogInfo::default()
            });
        }''' if candidate else '')
    entry = entry.replace('__PROCESS__', 'process_pending_logged' if candidate else 'process_pending')
    entry = entry.replace('__LOG_ARG__', ', if diagnostics { Some(&mut logger) } else { None }' if candidate else '')
    entry = entry.replace('__CHECKPOINT__', 'persist_checkpoint_logged(&db, &mut pending, if diagnostics { Some(&mut logger) } else { None })' if candidate else 'persist_checkpoint(&db, &mut pending)')
    # Insert inside the actual macos_impl, immediately before its existing
    # cfg(test) batch helper. All production decisions remain unchanged.
    marker = '    #[cfg(test)]\n    fn batch_action('
    shared.require(text.count(marker) == 1, 'Cannot locate unambiguous private-module insertion point')
    text = text.replace(marker, entry + '\n' + marker)
    text += '\n#[cfg(target_os = "macos")]\npub use macos_impl::round8_entry;\n'
    output.mkdir()
    injected = output / 'watch.injected.rs'
    injected.write_text(text)
    # ExcludeMatcher is pub(crate). Compile the matching production scanner in
    # this private crate instead of widening its API or linking a foreign matcher.
    scanner_source = source_root / 'src/scanner.rs'
    scanner_copy = output / 'scanner.rs'
    scanner_copy.write_bytes(scanner_source.read_bytes())
    driver = output / 'driver.rs'
    driver.write_text('pub use macevery::{db, error, model, process_lifetime};\n'
                      '#[path="scanner.rs"] mod scanner;\n'
                      '#[path="watch.injected.rs"] mod watch;\n'
                      'fn main() { if let Err(e) = watch::round8_entry(&std::env::args().skip(1).collect::<Vec<_>>()) { eprintln!("{e}"); std::process::exit(1); } }\n')
    binary = output / 'watch-recovery'
    command = ['rustc', '--edition=2021', '-O', '-C', 'lto=fat', '--crate-name', 'round8_watch', str(driver),
               '--extern', 'macevery=' + str(library), '-o', str(binary)]
    with (output / 'build.stdout').open('wb') as stdout, (output / 'build.stderr').open('wb') as stderr:
        subprocess.run(command, stdout=stdout, stderr=stderr, check=True, timeout=180)
    return binary, {'source_watch_sha256': shared.sha256(source), 'source_scanner_sha256': shared.sha256(scanner_source),
                    'copied_scanner_sha256': shared.sha256(scanner_copy), 'library_sha256': shared.sha256(library),
                    'injected_watch_sha256': shared.sha256(injected), 'driver_sha256': shared.sha256(driver),
                    'binary_sha256': shared.sha256(binary), 'written_rows_semantics': 'actual_outer_files_changes' if candidate else 'visited_proxy_not_actual_SQLite_changes'}


class Owned:
    """Only this object calls wait4; unreaped leader pins its PID/PGID identity."""
    def __init__(self, command, directory, timeout):
        self.stderr = (directory / 'stderr.log').open('wb')
        try:
            self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                            stderr=self.stderr, start_new_session=True)
        except BaseException:
            self.stderr.close()
            raise
        self.pid = self.process.pid
        self.deadline = time.monotonic() + timeout
        self.reaped = False
        self.receipt = None
        self.lines = []

    def read(self, phase):
        remaining = self.deadline - time.monotonic()
        shared.require(remaining > 0 and select.select([self.process.stdout], [], [], remaining)[0], 'Owned harness phase timeout')
        line = self.process.stdout.readline(64 * 1024)
        shared.require(line and len(line) < 64 * 1024 and line.endswith(b'\n'), 'Owned harness EOF/oversized response')
        value = json.loads(line)
        self.lines.append(value)
        shared.require(value.get('phase') == phase, 'Unexpected harness phase')
        return value

    def go(self):
        self.process.stdin.write(b'go\n')
        self.process.stdin.flush()

    def reap(self, wait_seconds=3):
        deadline = time.monotonic() + wait_seconds
        while not self.reaped:
            pid, status, usage = os.wait4(self.pid, os.WNOHANG)
            if pid:
                self.reaped = True
                self.process.returncode = os.waitstatus_to_exitcode(status)
                self.receipt = {'pid': self.pid, 'exit_code': self.process.returncode,
                                'lifetime_cpu_seconds': usage.ru_utime + usage.ru_stime,
                                'lifetime_peak_rss_bytes': usage.ru_maxrss, 'reaped': True}
                break
            if time.monotonic() >= deadline:
                raise TimeoutError('Owned harness did not exit')
            time.sleep(.01)
        return self.receipt

    def close(self):
        if not self.reaped:
            for sig in [signal.SIGTERM, signal.SIGKILL]:
                try:
                    if os.getpgid(self.pid) == self.pid:
                        os.killpg(self.pid, sig)
                except ProcessLookupError:
                    pass
                try:
                    self.reap(2)
                    break
                except TimeoutError:
                    continue
        for stream in [self.process.stdin, self.process.stdout, self.stderr]:
            try:
                stream.close()
            except OSError:
                pass
        return self.receipt or {'pid': self.pid, 'reaped': False}


def sample_summary(samples):
    good = [item for item in samples if 'rss_bytes' in item]
    return {'samples': len(good), 'sampled_peak_rss_bytes': max((s['rss_bytes'] for s in good), default=None),
            'sampled_peak_footprint_bytes': max((s['physical_footprint_bytes'] for s in good), default=None),
            'start_rss_bytes': good[0]['rss_bytes'] if good else None,
            'end_rss_bytes': good[-1]['rss_bytes'] if good else None,
            'start_footprint_bytes': good[0]['physical_footprint_bytes'] if good else None,
            'end_footprint_bytes': good[-1]['physical_footprint_bytes'] if good else None}


def run_one(binary, mother, roots, scenario, logging, folder, metrics, args):
    folder.mkdir()
    copy = shared.clone_or_backup(mother, folder / 'index.sqlite', args.timeout)
    shared.require(copy['method'].startswith('APFS_clonefile'), 'Fresh APFS clone required for comparable input layout')
    write_json(folder / 'copy.private.json', copy)
    owned = Owned([str(binary), 'run', str(folder / 'index.sqlite'), *(str(p) for p in roots), scenario, logging], folder, args.timeout)
    sampler = shared.Sampler(owned.pid, args.sample_interval, metrics)
    result = {'scenario': scenario, 'logging': logging, 'injected_flags': {'must': 1, 'dropped': 3, 'ordinary': 0}[scenario],
              'injected_event_id': 101}
    try:
        owned.read('ready')
        sampler.phase = 'scan'
        sampler.capture()
        owned.go()
        result.update(owned.read('measured'))
        sampler.capture()
        scan_samples = [dict(s) for s in sampler.samples if s['phase'] == 'scan']
        sampler.phase = 'validation'
        owned.go()
        result.update(owned.read('validated'))
        # Stop sampling before wait4 releases the leader PID for possible reuse.
        sampler.close()
        result['process'] = owned.reap()
        shared.require(result['process']['exit_code'] == 0, 'Harness nonzero exit')
        result['scan_memory'] = sample_summary(scan_samples)
        shared.require(result['scan_memory']['samples'] >= 2, 'No bounded scan memory endpoints')
        shared.require(result['all_fields_and_ids_equal_except_indexed_at'], 'Record equivalence failed')
        shared.require(result['revision_after'] - result['revision_before'] == result['indexed_at_changed_rows'], 'Unexpected journal/content revision amplification')
        return result
    finally:
        sampler.close()
        result['cleanup'] = owned.close()
        result['cleanup']['unreaped_owned_leader_remaining'] = not result['cleanup']['reaped']
        write_json(folder / 'responses.private.json', owned.lines)
        write_json(folder / 'samples.private.json', sampler.samples)
        write_json(folder / 'result.json', result)
        shared.require(result['cleanup']['reaped'], 'Owned harness cleanup failed; refusing subsequent runs')


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--baseline-source', type=Path, required=True)
    parser.add_argument('--baseline-library', type=Path, required=True)
    parser.add_argument('--candidate-source', type=Path, required=True)
    parser.add_argument('--candidate-library', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--files-per-root', type=int, default=3000)
    parser.add_argument('--subtree-files', type=int, default=100)
    parser.add_argument('--blocks', type=int, default=2)
    parser.add_argument('--timeout', type=float, default=60)
    parser.add_argument('--sample-interval', type=float, default=.02)
    args = parser.parse_args()
    shared.require(sys.platform == 'darwin', 'macOS only')
    shared.require(1 <= args.subtree_files < args.files_per_root <= 20_000, 'Fixture bounds: 1 <= subtree < root <= 20000')
    shared.require(1 <= args.blocks <= 5 and args.timeout > 0 and .005 <= args.sample_interval <= .1, 'Invalid bounded measurement settings')
    output = shared.require_tmp(args.output)
    shared.require(not output.exists(), 'Output must be new; failed evidence is never overwritten')
    output.mkdir(mode=0o700, parents=True)
    report = {'version': 1, 'passed': False, 'runs': [], 'scope': 'private production flag injection, unchanged real FS; not FSEvents delivery',
              'input': {'files_per_root': args.files_per_root, 'subtree_files': args.subtree_files, 'blocks': args.blocks},
              'metrics': {'scan_cpu': 'getrusage user+system microseconds, all process threads; excludes preparation/validation',
                          'scan_memory': 'libproc resident and phys_footprint endpoints/20ms samples, lower-bound peaks',
                          'lifetime': 'wait4 CPU/HWM includes initialization and complete record validation',
                          'cache': 'fresh APFS copies preserve physical layout; no cold-cache claim'},
              'source_rlib_correspondence': 'caller must independently freeze/prove this; SHA pins recorded'}
    try:
        binaries = {}
        report['artifacts'] = {}
        for name, is_new in [('baseline', False), ('candidate', True)]:
            binaries[name], report['artifacts'][name] = build_harness(
                getattr(args, name + '_source').resolve(), getattr(args, name + '_library').resolve(), output / name, is_new)
        roots = [output / 'filesystem/root-a', output / 'filesystem/root-b']
        for i, root in enumerate(roots):
            root.mkdir(parents=True)
            for n in range(args.files_per_root):
                parent = root / 'target' if i == 0 and n < args.subtree_files else root
                parent.mkdir(exist_ok=True)
                (parent / f'{n:06}-文件.txt').write_text(f'fixed fixture {i} {n}\n')
        template = output / 'mother/index.sqlite'
        template.parent.mkdir()
        seed_folder = output / 'seed'
        seed_folder.mkdir()
        seed = Owned([str(binaries['candidate']), 'seed', str(template), *(str(p) for p in roots)], seed_folder, args.timeout)
        try:
            seed.read('seeded')
            report['seed_process'] = seed.reap()
            shared.require(report['seed_process']['exit_code'] == 0, 'Fixture seed failed')
        finally:
            report['seed_cleanup'] = seed.close()
            shared.require(report['seed_cleanup']['reaped'], 'Seed cleanup failed')
        shared.require(not shared.nonempty_wal(template), 'Mother WAL must be empty after seed connection closes')
        mother_sha = shared.sha256(template)
        report['mother_sha256'] = mother_sha
        metrics = shared.ProcessMetrics()
        report['mach_timebase'] = {'numer': metrics.timebase.numer, 'denom': metrics.timebase.denom}
        for block in range(args.blocks):
            for scenario in ['must', 'dropped', 'ordinary']:
                for group, sequence in [('code_scope', [('baseline', 'off'), ('candidate', 'off'), ('baseline', 'off')]),
                                        ('diagnostics', [('candidate', 'off'), ('candidate', 'on'), ('candidate', 'off')])]:
                    for position, (name, logging) in enumerate(sequence):
                        shared.require(shared.sha256(template) == mother_sha, 'Mother DB changed before clone')
                        label = f'b{block}-{scenario}-{group}-{position}-{name}-{logging}'
                        result = run_one(binaries[name], template, roots, scenario, logging, output / label, metrics, args)
                        result.update(block=block, group=group, position=position, version=name)
                        expected_scoped = scenario == 'ordinary' or (name == 'candidate' and scenario == 'must')
                        shared.require(result['neighbor_full_records_unchanged'] == expected_scoped, 'Actual production recovery range differs from contract')
                        expected_visited = args.subtree_files + 1 if expected_scoped else 2 * args.files_per_root + 3
                        shared.require(result['visited'] == expected_visited, 'Actual visited count differs from fixture shape')
                        report['runs'].append(result)
                        write_json(output / 'summary.json', report)
        shared.require(shared.sha256(template) == mother_sha, 'Mother DB changed after all runs')
        report['mother_unchanged'] = True
        report['all_owned_reaped'] = all(run['cleanup']['reaped'] for run in report['runs']) and report['seed_cleanup']['reaped']
        report['passed'] = True
    except Exception as error:
        report['failure'] = {'type': type(error).__name__, 'message': str(error)}
        raise
    finally:
        write_json(output / 'summary.json', report)
    print(json.dumps({'passed': report['passed'], 'runs': len(report['runs']), 'summary': str(output / 'summary.json')}, ensure_ascii=False))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
