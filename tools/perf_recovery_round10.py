#!/usr/bin/env python3
"""Root-scheduled production recovery4dc/candidate A/B/A on identical small trees.

Reuse the round9 private-module entry and owned-process controller, with the
actual EventInbox queue on BOTH sides (4dc already fixed queue boundaries).
Baseline preserves legacy fresh-generation writes. Candidate must preserve all
13 fields for unchanged files and delete the same stale row, with zero files
writes and one delete revision. Generic upsert semantics are not changed here.
No actual OS drop, App, live DB, allocator purge or production resource sampler.
"""
import argparse
import copy
import hashlib
from pathlib import Path
import subprocess
import sys

import check_service_round2 as shared
import perf_watch_round9 as old

OWNED_CHILDREN = []
OriginalOwned = old.Owned


class TrackedOwned(OriginalOwned):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        OWNED_CHILDREN.append(self)


# Also covers a sampler-construction failure before the inherited run_one try.
old.Owned = TrackedOwned


def entry(candidate):
    source = old.ENTRY
    start = '        let generation = after[0].indexed_at;'
    end = '        assert_eq!(db.roots_and_excludes()?, configuration, "roots/excludes changed");'
    shared.require(source.count(start) == source.count(end) == 1, 'Pinned entry boundaries changed')
    if candidate:
        replacement = '''        assert!(expected.iter().zip(&after).all(|(a,b)| **a == *b),
            "candidate scanner no-op must preserve all13 fields and IDs");
'''
        source = source.split(start)[0] + replacement + end + source.split(end)[1]
        needle = '        assert_eq!(stats.written, stats.indexed, "generation updates must remain writes");'
        shared.require(source.count(needle) == 1, 'Pinned write assertion changed')
        source = source.replace(needle, '''        assert_eq!(stats.written, 0, "unchanged scanner must not write files");
        assert_eq!(stats.unchanged, stats.indexed);
''')
        shared.require(source.count('fresh_generation_all_rows') == 1, 'Pinned generation label changed')
        source = source.replace('fresh_generation_all_rows', 'unchanged_scanner_all_13_fields_preserved')
    else:
        # 4dc uses the same production inbox as candidate: one pre-boundary scan.
        shared.require('generation updates must remain writes' in source, 'Legacy assertion absent')
    return source


def build(args, output, variant):
    root = output / ('build-' + variant)
    root.mkdir()
    adapted = copy.copy(args)
    adapted.candidate_source = args.candidate_source if variant == 'candidate' else args.baseline_source
    adapted.candidate_library = args.candidate_library if variant == 'candidate' else args.baseline_library
    # Compile verbatim db/sqlite in the same private driver crate too. Scanner
    # legitimately uses pub(crate) seen-ID APIs; widening production visibility
    # or calling a second external-crate Database would test a different path.
    source_root = adapted.candidate_source.resolve(strict=True)
    library = adapted.candidate_library.resolve(strict=True)
    source = {name: (source_root / 'src' / name).read_bytes()
              for name in ['watch.rs', 'scanner.rs', 'db.rs', 'sqlite.rs']}
    watch = source['watch.rs'].decode()
    shared.require('struct EventInbox {' in watch and 'fn merge_incoming(' in watch,
                   'Both variants must use the production inbox')
    injected = entry(variant == 'candidate').replace('__QUEUE__', old.CANDIDATE_QUEUE).replace('__PREEXISTING_SCANS__', '1')
    bounded = 'fn enforce_worker_plan_bound(' in watch
    if bounded:
        shared.require('enforce_worker_plan_bound(&mut pending, &roots)' in watch, 'Production cap call shape changed')
    for marker in ['__BOUND_BEFORE__', '__BOUND_AFTER__']:
        injected = injected.replace(marker, 'assert!(enforce_worker_plan_bound(&mut pending, &roots), "owned fixture scope must remain valid");' if bounded else '')
    marker = '    #[cfg(test)]\n    fn batch_action('
    shared.require(watch.count(marker) == 1, 'Private insertion point changed')
    watch = watch.replace(marker, injected + '\n' + marker)
    watch += '\n#[cfg(target_os="macos")]\npub use macos_impl::round9_entry;\n'
    for name in ['scanner.rs', 'db.rs', 'sqlite.rs']:
        (root / name).write_bytes(source[name])
    (root / 'watch.injected.rs').write_text(watch)
    driver = root / 'driver.rs'
    driver.write_text('pub use macevery::{error,model,process_lifetime};\n'
                      '#[path="sqlite.rs"] mod sqlite;\n#[path="db.rs"] mod db;\n'
                      '#[path="scanner.rs"] mod scanner;\n#[path="watch.injected.rs"] mod watch;\n'
                      'fn main(){if let Err(e)=watch::round9_entry(&std::env::args().skip(1).collect::<Vec<_>>())'
                      '{eprintln!("{e}");std::process::exit(1);}}\n')
    binary = root / 'watch-mechanism'
    command = ['rustc', '--edition=2021', '-O', '-C', 'lto=fat', '--crate-name', 'round10_recovery',
               str(driver), '--extern', 'macevery=' + str(library), '-o', str(binary)]
    with (root / 'build.log').open('x') as log:
        subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=True, timeout=180)
    metadata = {'source_sha256': {name: hashlib.sha256(value).hexdigest() for name, value in source.items()},
                'injected_watch_sha256': shared.sha256(root / 'watch.injected.rs'),
                'driver_sha256': shared.sha256(driver), 'library': shared.identity(library, True),
                'binary': shared.identity(binary, True), 'command': command,
                'production_worker_plan_cap_called': bounded}
    metadata['queue_contract'] = 'Actual EventInbox::take_batch and PendingWork::merge_incoming on both sides'
    metadata['scanner_contract'] = 'unchanged all13 fields retained' if variant == 'candidate' else 'fresh generation writes'
    return binary, metadata


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--baseline-source', type=Path, required=True)
    p.add_argument('--candidate-source', type=Path, default=Path(__file__).resolve().parents[1])
    p.add_argument('--baseline-library', type=Path, required=True)
    p.add_argument('--candidate-library', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--files-per-root', type=int, default=3000)
    p.add_argument('--subtree-files', type=int, default=100)
    p.add_argument('--events', type=int, default=9000)
    p.add_argument('--blocks', type=int, default=2)
    p.add_argument('--timeout', type=float, default=90)
    p.add_argument('--sample-interval', type=float, default=.02)
    args = p.parse_args()
    args.repo = Path(__file__).resolve().parents[1]
    shared.require(sys.platform == 'darwin' and 1 <= args.blocks <= 4
                   and 1 <= args.subtree_files < args.files_per_root <= 20000
                   and 8195 <= args.events <= 12288 and 5 <= args.timeout <= 300
                   and .005 <= args.sample_interval <= .1, 'Owned benchmark bounds exceeded')
    output = shared.require_tmp(args.output)
    shared.require(not output.exists(), 'Evidence must be fresh')
    output.mkdir(mode=0o700, parents=True)
    report = {'status': 'FAIL', 'runs': [], 'artifacts': {}, 'controller_sha256': shared.sha256(Path(__file__)),
              'base_controller_sha256': shared.sha256(Path(old.__file__)),
              'notes': ['4dc and candidate both coalesce pre-existing markers into one scan; no queue3to1 saving claimed.',
                        'Measures combined candidate recovery/DB path versus4dc with the same inbox contract; not unique CPU attribution to one changed function.',
                        'Measured scan CPU excludes ingress, validation and checkpoint; processing/lifetime costs separate.',
                        'True filesystem small tree, same mother APFS clones, no cache-cold claim.',
                        'No real FSEvents pressure/drop, App, live DB or old observation data used.']}
    try:
        binaries = {}
        for variant in ['baseline', 'candidate']:
            binaries[variant], report['artifacts'][variant] = build(args, output, variant)
        roots = [output/'filesystem/root-a', output/'filesystem/root-b']
        for ordinal, root in enumerate(roots):
            root.mkdir(parents=True)
            for number in range(args.files_per_root):
                parent = root/'target' if ordinal == 0 and number < args.subtree_files else root
                parent.mkdir(exist_ok=True)
                (parent/f'{number:06}-文件.txt').write_text(f'fixed fixture {ordinal} {number}\n')
        mother = output/'mother/index.sqlite';mother.parent.mkdir()
        folder = output/'seed';folder.mkdir()
        child = old.Owned([str(binaries['candidate']), 'seed', str(mother), *map(str, roots)], folder, args.timeout)
        try:
            child.read('seeded');report['seed_process'] = child.reap()
            shared.require(report['seed_process']['exit_code'] == 0, 'Fixture seed failed')
        finally:
            report['seed_cleanup'] = child.close()
            shared.require(report['seed_cleanup']['reaped'], 'Seed unreaped')
        shared.require(not shared.nonempty_wal(mother), 'Mother is not closed')
        report['mother_sha256'] = shared.sha256(mother)
        metrics = shared.ProcessMetrics()
        for block in range(args.blocks):
            for scenario in ['preexisting', 'postboundary', 'retry']:
                for position, variant in enumerate(['baseline', 'candidate', 'baseline']):
                    folder = output/f'b{block}-{scenario}-{position}-{variant}'
                    result = old.run_one(binaries[variant], mother, roots, folder, scenario, 'off', args, metrics)
                    result.update(block=block, scenario=scenario, position=position, variant=variant)
                    memory = result['processing_memory']
                    shared.require(memory['samples'] >= 2 and all(
                        type(memory.get(key)) is int and memory[key] > 0 for key in
                        ['start_rss_bytes','end_rss_bytes','start_footprint_bytes','end_footprint_bytes']),
                        'Correctness passed but resource endpoints missing; memory gate is not PASS')
                    shared.require(result['root_scans'] == (2 if scenario == 'postboundary' else 1), 'Recovery boundary regression')
                    shared.require(result['records'] == args.files_per_root*2+3, 'Whole tree count mismatch')
                    shared.require(result['rows_written'] == (0 if variant == 'candidate' else result['visited']), 'Generation write contract mismatch')
                    report['runs'].append(result);shared.write_json(output/'summary.local.json', report)
                    print(folder.name + ': PASS', flush=True)
        shared.require(shared.sha256(mother) == report['mother_sha256'], 'Mother changed')
        report.update(status='PASS', all_owned_reaped=True, mother_unchanged=True)
    except BaseException as error:
        report['failure'] = {'type':type(error).__name__, 'message':str(error)}
        raise
    finally:
        report['outer_owned_cleanup'] = [child.close() for child in OWNED_CHILDREN]
        shared.write_json(output/'summary.local.json', report)
        shared.require(all(item['reaped'] for item in report['outer_owned_cleanup']), 'Outer owned cleanup failed')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
