#!/usr/bin/env python3
"""Root-scheduled deterministic production watcher recovery A/B/A, small tree.

  python3 tools/perf_watch_round9.py \
    --baseline-library .build/watch-recovery-20261020/candidate-frozen/libmacevery.rlib \
    --candidate-library target/release/libmacevery.rlib \
    --output /private/tmp/macevery-watch-round9-UNIQUE

Default baseline watch/scanner sources come from exact git commit 29b6140.
Every version compiles its own copied production watch/scanner modules, linking
the corresponding supplied rlib. A tool entry is injected into macos_impl only.
Baseline's inline receive/drain block is extracted verbatim, never reimplemented;
candidate calls actual EventInbox::take_batch and PendingWork::merge_incoming.
Production process_pending_logged/refresh_roots/checkpoint paths handle work.
The caller must separately prove each library corresponds to its pinned source.

Scenarios: 9000 pre-admitted events, UserDropped at offsets 0/4097/8194;
8195..12288 events are supported: these fixed markers cover three 4096-event batches.
an independent after-boundary drop at first rescan-closure entry; a failed root
closure then retry proving the checkpoint stays blocked. The after-boundary
delivery precedes the filesystem walk, not an actual concurrent OS callback.
This demonstrates the pre-existing queue mechanism, not the historical cause
of UserDropped, and cannot prove that any historic recovery was unnecessary.

Two roots contain 3000 files each plus one directory (6003 live records), and
one stale SQL-only row tests cleanup. Same physical mother, fresh APFS clones,
unchanged real tree; generation changes remain observable writes/revisions.
CPU/wall measured inside actual refresh_roots closures exclude queueing, logs,
checkpoint and validation; processing/ingress/lifetime costs are separate.
20ms RSS/footprint peaks include controller-labelled processing phase and its
small bookkeeping; wait4 CPU/RSS includes the whole process and full validation.
No actual FSEvents stream, App, live index, system pressure or cache purge.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

import check_service_round2 as shared
from perf_watch_recovery_round8 import Owned, sample_summary

BASELINE = '29b6140575cdd69c965732b943dccda77e8b586e'

ENTRY = r'''
    #[derive(Default)]
    struct Round9Stats {
        attempts: usize, scans: usize, failures: usize, indexed: u64,
        written: u64, unchanged: u64, deleted: u64, warnings: usize,
        scan_cpu_us: i64, scan_wall_us: u128,
    }
    #[repr(C)]
    #[derive(Default)]
    struct Round9Timeval { sec: i64, usec: i32, padding: i32 }
    #[repr(C)]
    #[derive(Default)]
    struct Round9Rusage { user: Round9Timeval, system: Round9Timeval, maxrss: i64, rest: [i64; 13] }
    extern "C" { fn getrusage(who: i32, out: *mut Round9Rusage) -> i32; }
    fn round9_cpu_us() -> i64 {
        let mut out = Round9Rusage::default();
        assert_eq!(unsafe { getrusage(0, &mut out) }, 0);
        (out.user.sec + out.system.sec) * 1_000_000 + i64::from(out.user.usec) + i64::from(out.system.usec)
    }
    fn round9_gate() {
        let mut line = String::new();
        assert!(std::io::stdin().read_line(&mut line).unwrap() > 0);
        assert_eq!(line.trim(), "go");
    }
    fn round9_print(value: &str) {
        use std::io::Write;
        assert!(value.len() < 32 * 1024, "bounded JSON event");
        println!("{value}"); std::io::stdout().flush().unwrap();
    }
    __QUEUE__
    pub fn round9_entry(args: &[String]) -> crate::error::Result<()> {
        let db_path = PathBuf::from(&args[1]);
        assert!(db_path.starts_with("/private/tmp"), "isolated output only");
        let roots = [PathBuf::from(&args[2]), PathBuf::from(&args[3])];
        assert!(roots.iter().all(|p| p.starts_with("/private/tmp")));
        let db = Database::open(db_path.clone())?;
        if args[0] == "seed" {
            crate::scanner::index_paths(&db, &crate::scanner::IndexRequest {
                roots: roots.to_vec(), excludes: vec![".git".into()], rebuild: true,
            })?;
            db.upsert_file(&crate::model::FileRecord::new(
                roots[0].join("stale-round9-missing.txt").to_string_lossy().into_owned(),
                crate::model::FileKind::File, 1))?;
            db.set_meta(META_LAST_EVENT_ID, "100")?;
            round9_print("{\"phase\":\"seeded\"}");
            return Ok(());
        }
        assert_eq!(args[0], "run");
        let scenario = args[4].as_str();
        let logging = args[5] == "on";
        let events: usize = args[6].parse().unwrap();
        assert!((8195..=12288).contains(&events));
        assert!(matches!(scenario, "preexisting" | "postboundary" | "retry"));
        let before = db.all_records()?;
        let stale = roots[0].join("stale-round9-missing.txt").to_string_lossy().into_owned();
        assert!(!Path::new(&stale).exists());
        assert_eq!(before.iter().filter(|r| r.path == stale).count(), 1);
        let expected = before.iter().filter(|r| r.path != stale).collect::<Vec<_>>();
        let stamp_before = db.content_stamp()?;
        let configuration = db.roots_and_excludes()?;
        assert_eq!(db.get_meta(META_LAST_EVENT_ID)?.as_deref(), Some("100"));
        let excludes = runtime_excludes(vec![".git".into()], &db_path);
        let filter = EventFilter::new(&db_path, &excludes);
        let hash_state = RandomState::new();
        let queue = Round9Queue::new(roots.to_vec(), hash_state.clone());
        let mut pending = PendingWork { hash_state, ..PendingWork::default() };
        let stats = std::cell::RefCell::new(Round9Stats::default());
        let checkpoints = std::cell::RefCell::new(Vec::<u64>::new());
        let mut logger = TriggerLog::new(&db_path);
        round9_print("{\"phase\":\"ready\"}");
        round9_gate();
        let ingress_cpu = round9_cpu_us();
        let ingress_wall = Instant::now();
        let admitted = if scenario == "preexisting" { events } else { 1 };
        for offset in 0..admitted {
            let event = FsEvent {
                path: roots[0].join(format!("admitted-event-{offset:05}")).to_string_lossy().into_owned(),
                flags: if [0, 4097, 8194].contains(&offset) {
                    FLAG_MUST_SCAN_SUBDIRS | FLAG_USER_DROPPED
                } else { 0 },
                id: 101 + offset as u64,
            };
            assert!(filter.accepts(&event.path, event.flags));
            queue.push(event);
        }
        let ingress_wall_us = ingress_wall.elapsed().as_micros();
        let ingress_cpu_us = round9_cpu_us() - ingress_cpu;
        round9_print(&format!("{{\"phase\":\"ingress\",\"admitted\":{admitted},\"ingress_cpu_us\":{ingress_cpu_us},\"ingress_wall_us\":{ingress_wall_us}}}"));
        round9_gate();
        let processing_cpu = round9_cpu_us();
        let processing_wall = Instant::now();
        let mut batches = 0;
        let mut delayed_injected = false;
        let mut failure_blocked = false;
        let mut first_boundary_checkpoint = 0;
        loop {
            assert!(batches < 12, "unexpected non-termination");
            if queue.remaining.get() > 0 {
                queue.take_into(&mut pending, &roots);
            }
            if !pending.has_work() { break; }
            __BOUND_BEFORE__
            batches += 1;
            assert!(invalidate_wrapped_checkpoint(&db, &mut pending).is_ok());
            process_pending_logged(&mut pending, true,
                |_| panic!("all injected scenarios require global recovery"),
                || {
                    stats.borrow_mut().attempts += 1;
                    if scenario == "postboundary" && !delayed_injected {
                        // The first queue boundary is already detached. This is
                        // an admitted new loss, not old queued work to discard.
                        queue.push(FsEvent {
                            path: roots[0].to_string_lossy().into_owned(),
                            flags: FLAG_MUST_SCAN_SUBDIRS | FLAG_USER_DROPPED,
                            id: 102,
                        });
                        delayed_injected = true;
                    }
                    let fail_now = scenario == "retry" && stats.borrow().attempts == 1;
                    if fail_now {
                        stats.borrow_mut().failures += 1;
                        return Err(MacEveryError::Cli("round9 deterministic failed rescan closure".into()));
                    }
                    let cpu = round9_cpu_us();
                    let wall = Instant::now();
                    let result = refresh_roots(&db, &roots, &excludes);
                    let wall_us = wall.elapsed().as_micros();
                    let cpu_us = round9_cpu_us() - cpu;
                    let summary = result?;
                    let mut stats = stats.borrow_mut();
                    stats.scans += 1;
                    stats.scan_cpu_us += cpu_us;
                    stats.scan_wall_us += wall_us;
                    stats.indexed += summary.indexed;
                    stats.written += summary.rows_written;
                    stats.unchanged += summary.unchanged;
                    stats.deleted += summary.deleted;
                    stats.warnings += summary.warnings.len();
                    Ok(summary)
                }, if logging { Some(&mut logger) } else { None });
            __BOUND_AFTER__
            let complete = persist_checkpoint_logged(&db, &mut pending, if logging { Some(&mut logger) } else { None });
            let checkpoint: u64 = db.get_meta(META_LAST_EVENT_ID)?.unwrap().parse().unwrap();
            checkpoints.borrow_mut().push(checkpoint);
            if scenario == "postboundary" && batches == 1 {
                first_boundary_checkpoint = checkpoint;
                assert_eq!(checkpoint, 101);
                assert_eq!(queue.remaining.get(), 1, "post-boundary drop must remain queued");
                assert!(complete);
            }
            if scenario == "retry" && batches == 1 {
                assert!(!complete && checkpoint == 100 && pending.has_work());
                assert!(pending.retry_roots);
                assert_eq!(db.content_stamp()?.revision, stamp_before.revision);
                failure_blocked = true;
            } else {
                assert!(complete, "successful scans/checkpoints must complete");
            }
            if queue.remaining.get() == 0 && !pending.has_work() { break; }
        }
        let processing_wall_us = processing_wall.elapsed().as_micros();
        let processing_cpu_us = round9_cpu_us() - processing_cpu;
        let stats = stats.borrow();
        round9_print(&format!(concat!(
            "{{\"phase\":\"measured\",\"batches\":{},\"root_attempts\":{},\"root_scans\":{},\"injected_failures\":{},",
            "\"scan_cpu_us\":{},\"scan_wall_us\":{},\"processing_cpu_us\":{},\"processing_wall_us\":{},",
            "\"visited\":{},\"rows_written\":{},\"unchanged\":{},\"stale_removed\":{},\"warnings\":{}}}"),
            batches, stats.attempts, stats.scans, stats.failures, stats.scan_cpu_us, stats.scan_wall_us,
            processing_cpu_us, processing_wall_us, stats.indexed, stats.written, stats.unchanged, stats.deleted, stats.warnings));
        round9_gate();
        let after = db.all_records()?;
        let stamp_after = db.content_stamp()?;
        let all_fields = expected.len() == after.len() && expected.iter().zip(&after).all(|(old, new)| {
            let mut row: crate::model::FileRecord = (**old).clone(); row.indexed_at = new.indexed_at; row == *new
        });
        assert!(all_fields, "full ordered 13-field/ID mismatch except fresh indexed_at");
        assert!(!after.is_empty());
        let generation = after[0].indexed_at;
        assert!(after.iter().all(|r| r.indexed_at == generation));
        assert!(expected.iter().zip(&after).all(|(a,b)| a.indexed_at != b.indexed_at));
        assert_eq!(db.roots_and_excludes()?, configuration, "roots/excludes changed");
        assert_eq!(stats.warnings, 0);
        assert_eq!(stats.deleted, 1);
        assert_eq!(stats.indexed, after.len() as u64 * stats.scans as u64);
        assert_eq!(stats.written, stats.indexed, "generation updates must remain writes");
        assert_eq!(stamp_after.revision - stamp_before.revision, (stats.written + stats.deleted) as i64);
        assert!(!pending.has_pending() && queue.remaining.get() == 0);
        let checkpoint: u64 = db.get_meta(META_LAST_EVENT_ID)?.unwrap().parse().unwrap();
        let expected_checkpoint = if scenario == "preexisting" { events as u64 + 100 } else if scenario == "postboundary" { 102 } else { 101 };
        assert_eq!(checkpoint, expected_checkpoint);
        let expected_scans = if scenario == "preexisting" { __PREEXISTING_SCANS__ } else if scenario == "postboundary" { 2 } else { 1 };
        assert_eq!(stats.scans, expected_scans);
        assert_eq!(stats.failures, usize::from(scenario == "retry"));
        if scenario == "retry" { assert!(failure_blocked); }
        round9_print(&format!(concat!(
            "{{\"phase\":\"validated\",\"records\":{},\"checkpoint\":{},\"checkpoints\":{:?},",
            "\"first_boundary_checkpoint\":{},\"all_fields_and_ids_equal_except_indexed_at\":true,",
            "\"fresh_generation_all_rows\":true,\"roots_and_excludes_unchanged\":true,",
            "\"stale_cleanup_preserved\":true,\"failed_checkpoint_blocked\":{},",
            "\"revision_before\":{},\"revision_after\":{},\"queue_remaining\":0}}"),
            after.len(), checkpoint, *checkpoints.borrow(), first_boundary_checkpoint,
            failure_blocked, stamp_before.revision, stamp_after.revision));
        Ok(())
    }
'''

BASELINE_QUEUE = r'''
    struct Round9Queue {
        sender: Sender<FsEvent>, receiver: Receiver<FsEvent>,
        remaining: std::cell::Cell<usize>,
    }
    impl Round9Queue {
        fn new(_: Vec<PathBuf>, _: RandomState) -> Self {
            let (sender, receiver) = mpsc::channel();
            Self { sender, receiver, remaining: std::cell::Cell::new(0) }
        }
        fn push(&self, event: FsEvent) {
            self.sender.send(event).unwrap(); self.remaining.set(self.remaining.get() + 1);
        }
        fn take_into(&self, pending: &mut PendingWork, roots: &[PathBuf]) {
            let receiver = &self.receiver;
            let retry_at: Option<Instant> = None;
            let before_count = pending.event_count;
            __ACTUAL_INLINE_RECEIVE_BLOCK__
            assert!(!disconnected);
            let received = (pending.event_count - before_count) as usize;
            assert!(received > 0 && received <= self.remaining.get());
            self.remaining.set(self.remaining.get() - received);
        }
    }
'''
CANDIDATE_QUEUE = r'''
    struct Round9Queue { inbox: EventInbox, remaining: std::cell::Cell<usize> }
    impl Round9Queue {
        fn new(roots: Vec<PathBuf>, hash_state: RandomState) -> Self {
            Self { inbox: EventInbox::new(roots, hash_state), remaining: std::cell::Cell::new(0) }
        }
        fn push(&self, event: FsEvent) {
            self.inbox.push(event); self.remaining.set(self.remaining.get() + 1);
        }
        fn take_into(&self, pending: &mut PendingWork, _: &[PathBuf]) {
            let incoming = self.inbox.take_batch(Duration::ZERO);
            let received = incoming.stats.admitted as usize;
            assert!(!incoming.closed && received > 0 && received <= self.remaining.get());
            self.remaining.set(self.remaining.get() - received);
            pending.merge_incoming(incoming.pending);
        }
    }
'''


def digest_text(value):
    return hashlib.sha256(value.encode()).hexdigest()


def source_bytes(repo, source_root, commit, relative):
    if source_root:
        return (source_root / relative).read_bytes()
    return subprocess.check_output(['git', 'show', commit + ':' + relative], cwd=repo)


def build(args, output, variant):
    candidate = variant == 'candidate'
    source_root = args.candidate_source if candidate else args.baseline_source
    commit = None if candidate else args.baseline_commit
    watch = source_bytes(args.repo, source_root, commit, 'src/watch.rs').decode()
    scanner = source_bytes(args.repo, source_root, commit, 'src/scanner.rs')
    original_watch_sha = digest_text(watch)
    library = (args.candidate_library if candidate else args.baseline_library).resolve(strict=True)
    fragment_sha = None
    if candidate:
        shared.require('struct EventInbox {' in watch and 'fn merge_incoming(' in watch,
                       'Candidate production inbox contract missing')
        queue = CANDIDATE_QUEUE
        expected_scans = '1'
    else:
        start = '                let first = if pending.has_pending() {'
        end = '                let retry_due ='
        shared.require(watch.count(start) == 1, 'Ambiguous baseline receive-block start')
        block = watch.split(start, 1)[1].split(end, 1)[0]
        block = start + block
        shared.require('for _ in 1..MAX_BATCH_EVENTS {' in block
                       and 'const MAX_BATCH_EVENTS: usize = 4096;' in watch,
                       'Baseline is not the frozen4096-event mechanism')
        fragment_sha = digest_text(block)
        queue = BASELINE_QUEUE.replace('__ACTUAL_INLINE_RECEIVE_BLOCK__', block)
        expected_scans = 'events.div_ceil(MAX_BATCH_EVENTS)'
    entry = ENTRY.replace('__QUEUE__', queue).replace('__PREEXISTING_SCANS__', expected_scans)
    # The optional worker-plan cap is a production function. If available, call
    # it at the same before/after positions as the actual worker, not a new rule.
    bounded = candidate and 'fn enforce_worker_plan_bound(' in watch
    if bounded:
        shared.require('enforce_worker_plan_bound(&mut pending, &roots)' in watch,
                       'Cannot prove production worker-plan cap call shape')
    for marker in ['__BOUND_BEFORE__', '__BOUND_AFTER__']:
        entry = entry.replace(marker, 'assert!(enforce_worker_plan_bound(&mut pending, &roots), "owned fixture scope must remain valid");' if bounded else '')
    marker = '    #[cfg(test)]\n    fn batch_action('
    shared.require(watch.count(marker) == 1, 'Unambiguous private insertion point missing')
    watch = watch.replace(marker, entry + '\n' + marker)
    watch += '\n#[cfg(target_os="macos")]\npub use macos_impl::round9_entry;\n'
    folder = output / variant
    folder.mkdir()
    (folder / 'watch.injected.rs').write_text(watch)
    (folder / 'scanner.rs').write_bytes(scanner)
    driver = folder / 'driver.rs'
    driver.write_text('pub use macevery::{db,error,model,process_lifetime};\n'
                      '#[path="scanner.rs"] mod scanner;\n'
                      '#[path="watch.injected.rs"] mod watch;\n'
                      'fn main(){if let Err(e)=watch::round9_entry(&std::env::args().skip(1).collect::<Vec<_>>())'
                      '{eprintln!("{e}");std::process::exit(1);}}\n')
    binary = folder / 'watch-mechanism'
    command = ['rustc', '--edition=2021', '-O', '-C', 'lto=fat', '--crate-name', 'round9_watch',
               str(driver), '--extern', 'macevery=' + str(library), '-o', str(binary)]
    with (folder / 'build.log').open('x') as log:
        subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=True, timeout=180)
    return binary, {'source_watch_sha256': original_watch_sha,
                    'source_scanner_sha256': hashlib.sha256(scanner).hexdigest(),
                    'baseline_actual_receive_fragment_sha256': fragment_sha,
                    'production_worker_plan_cap_called': bounded,
                    'injected_watch_sha256': shared.sha256(folder / 'watch.injected.rs'),
                    'driver_sha256': shared.sha256(driver), 'library': shared.identity(library, True),
                    'binary': shared.identity(binary, True), 'command': command}


def run_one(binary, mother, roots, folder, scenario, logging, args, metrics):
    folder.mkdir()
    copied = shared.clone_or_backup(mother, folder / 'index.sqlite', args.timeout)
    shared.require(copied['method'].startswith('APFS_clonefile'), 'Same-layout APFS copy required')
    shared.write_json(folder / 'copy.local.json', copied)
    child = Owned([str(binary), 'run', str(folder / 'index.sqlite'), *map(str, roots), scenario,
                   logging, str(args.events)], folder, args.timeout)
    sampler = shared.Sampler(child.pid, args.sample_interval, metrics)
    result = {'scenario': scenario, 'logging': logging, 'status': 'FAIL'}
    try:
        child.read('ready')
        sampler.phase = 'ingress'
        sampler.capture()
        child.go()
        result.update(child.read('ingress'))
        sampler.capture()
        result['ingress_memory'] = sample_summary([s for s in sampler.samples if s['phase'] == 'ingress'])
        sampler.phase = 'processing'
        sampler.capture()
        child.go()
        result.update(child.read('measured'))
        sampler.capture()
        result['processing_memory'] = sample_summary([s for s in sampler.samples if s['phase'] == 'processing'])
        sampler.phase = 'validation'
        child.go()
        result.update(child.read('validated'))
        sampler.close()
        result['process'] = child.reap()
        shared.require(result['process']['exit_code'] == 0, 'Owned private watcher failed')
        shared.require(result['all_fields_and_ids_equal_except_indexed_at']
                       and result['roots_and_excludes_unchanged'] and result['stale_cleanup_preserved']
                       and result['queue_remaining'] == 0, 'Private watcher invariants failed')
        result['status'] = 'PASS'
    finally:
        sampler.close()
        result['cleanup'] = child.close()
        shared.write_json(folder / 'events.local.json', child.lines)
        shared.write_json(folder / 'samples.local.json', sampler.samples)
        shared.write_json(folder / 'result.local.json', result)
        shared.require(result['cleanup']['reaped'], 'Owned leader not reaped; stop subsequent runs')
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--repo', type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument('--baseline-source', type=Path)
    parser.add_argument('--baseline-commit', default=BASELINE)
    parser.add_argument('--candidate-source', type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument('--baseline-library', type=Path, required=True)
    parser.add_argument('--candidate-library', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--compile-only', action='store_true')
    parser.add_argument('--files-per-root', type=int, default=3000)
    parser.add_argument('--subtree-files', type=int, default=100)
    parser.add_argument('--events', type=int, default=9000)
    parser.add_argument('--blocks', type=int, default=2)
    parser.add_argument('--diagnostics', action='store_true', help='Additional candidate off/on/off block, preexisting scenario only')
    parser.add_argument('--timeout', type=float, default=90)
    parser.add_argument('--sample-interval', type=float, default=.02)
    args = parser.parse_args()
    shared.require(sys.platform == 'darwin', 'macOS only')
    shared.require(1 <= args.blocks <= 4 and 8195 <= args.events <= 12288
                   and 1 <= args.subtree_files < args.files_per_root <= 20000
                   and 5 <= args.timeout <= 300 and .005 <= args.sample_interval <= .1,
                   'Fixture/measurement bounds exceeded')
    output = shared.require_tmp(args.output.resolve())
    shared.require(not output.exists(), 'Refusing to overwrite existing evidence')
    output.mkdir(mode=0o700, parents=True)
    report = {'version': 1, 'status': 'FAIL', 'runs': [], 'artifacts': {},
              'controller_sha256': shared.sha256(Path(__file__)),
              'arguments': {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
              'notes': ['No actual FSEvents drop is induced or historically attributed.',
                        'Preexisting drop offsets0/4097/8194 admitted before first boundary, default9000 events.',
                        'Postboundary scenario is independent: one initial event and one newly admitted drop at first rescan-closure entry.',
                        'Retry fault is deterministic closure failure before FS scan; production retry/checkpoint functions remain unchanged.',
                        'Root scan CPU/wall sum only actual refresh_roots calls; ingress, processing, validation and lifetime are distinct.',
                        'Processing includes batching idle waits, logging, checkpoint and tiny scalar checks; CPU/wall are not scan-only.',
                        'Optional diagnostics off/on/off covers production scan/checkpoint records; it does not emulate callback/filter-summary logging.',
                        'Sampled RSS/footprint and lifetime HWM include all owned allocations and may miss short peaks.',
                        'Fresh same-mother APFS copies preserve layout; no cold-cache claim.',
                        'All generation-only writes remain visible; no revision suppression.',
                        'Caller must prove libraries match source; manifest hashes alone do not establish that.']}
    try:
        binaries = {}
        for variant in ['baseline', 'candidate']:
            binaries[variant], report['artifacts'][variant] = build(args, output, variant)
        if args.compile_only:
            report.update(status='COMPILED_ONLY', no_measurement_process_run=True)
            return 0
        roots = [output / 'filesystem/root-a', output / 'filesystem/root-b']
        for ordinal, root in enumerate(roots):
            root.mkdir(parents=True)
            for number in range(args.files_per_root):
                parent = root / 'target' if ordinal == 0 and number < args.subtree_files else root
                parent.mkdir(exist_ok=True)
                (parent / f'{number:06}-文件.txt').write_text(f'fixed fixture {ordinal} {number}\n')
        mother = output / 'mother/index.sqlite'
        mother.parent.mkdir()
        folder = output / 'seed'
        folder.mkdir()
        child = Owned([str(binaries['candidate']), 'seed', str(mother), *map(str, roots)], folder, args.timeout)
        try:
            child.read('seeded')
            report['seed_process'] = child.reap()
            shared.require(report['seed_process']['exit_code'] == 0, 'Owned fixture seeding failed')
        finally:
            report['seed_cleanup'] = child.close()
            shared.require(report['seed_cleanup']['reaped'], 'Fixture seed leader unreaped')
        shared.require(not shared.nonempty_wal(mother), 'Mother must have no nonempty WAL')
        mother_sha = shared.sha256(mother)
        report['mother_sha256'] = mother_sha
        metrics = shared.ProcessMetrics()
        for block in range(args.blocks):
            groups = [('code', scenario, [('baseline', 'off'), ('candidate', 'off'), ('baseline', 'off')])
                      for scenario in ['preexisting', 'postboundary', 'retry']]
            if args.diagnostics:
                groups.append(('diagnostics', 'preexisting', [('candidate', 'off'), ('candidate', 'on'), ('candidate', 'off')]))
            for group, scenario, sequence in groups:
                for position, (variant, logging) in enumerate(sequence):
                    shared.require(shared.sha256(mother) == mother_sha, 'Prepared mother changed')
                    folder = output / f'b{block}-{group}-{scenario}-{position}-{variant}-{logging}'
                    run = run_one(binaries[variant], mother, roots, folder, scenario, logging, args, metrics)
                    run.update(block=block, group=group, position=position, variant=variant)
                    expected_scans = (args.events + 4095) // 4096 if variant == 'baseline' and scenario == 'preexisting' else 1
                    if scenario == 'postboundary':
                        expected_scans = 2
                    shared.require(run['root_scans'] == expected_scans, 'Production recovery count differs from mechanism contract')
                    shared.require(run['records'] == args.files_per_root * 2 + 3, 'Unexpected full-tree record count')
                    report['runs'].append(run)
                    shared.write_json(output / 'summary.local.json', report)
                    print(folder.name + ': PASS', flush=True)
        shared.require(shared.sha256(mother) == mother_sha, 'Mother changed after runs')
        report.update(status='PASS', mother_unchanged=True,
                      all_owned_reaped=all(r['cleanup']['reaped'] for r in report['runs']))
    except BaseException as error:
        report['failure'] = {'type': type(error).__name__, 'message': str(error)}
        raise
    finally:
        shared.write_json(output / 'summary.local.json', report)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
