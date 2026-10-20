#!/usr/bin/env python3
"""Root-scheduled same-mother A/B/A prepared-upsert comparison, owned copies only.

CPU/wall are the Rust driver's transaction timers. Sampled RSS/footprint include
preload and full-field verification and can miss peaks; they are not allocator
or physical-write-byte counters. No live index, watcher or App is used.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import signal
import sqlite3
import subprocess
import threading
import time

from compare_round1 import clone_snapshot
from observe_app_resources import NativeProbe


def sha(path):
    digest = hashlib.sha256()
    with open(path, 'rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline-lib', type=Path, required=True)
    parser.add_argument('--candidate-lib', type=Path, required=True)
    parser.add_argument('--seed-cli', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--rows', type=int, default=100000)
    args = parser.parse_args()
    out = args.output.resolve()
    if not str(out).startswith('/private/tmp/macevery-watch-recovery-20261020/') or out.exists():
        parser.error('Choose a new directory under the owned round-8 temporary root')
    if not 1000 <= args.rows <= 300000:
        parser.error('rows must be within 1000..300000')
    out.mkdir(mode=0o700, parents=True)
    driver = Path(__file__).with_name('perf_upsert_round8.rs').resolve()
    manifest = {'driver_sha256': sha(driver), 'rows': args.rows, 'order': ['A', 'B', 'A'],
                'libraries': {}, 'seed_cli_sha256': sha(args.seed_cli),
                'cache_policy': 'same physical mother, fresh APFS clones, no cache flush'}
    binaries = {}
    for label, library in [('A', args.baseline_lib), ('B', args.candidate_lib)]:
        binary = out / ('driver-' + label)
        command = ['rustc', '--edition=2021', '-O', '-C', 'lto=fat', '--extern',
                   'macevery=' + str(library.resolve()), str(driver), '-o', str(binary)]
        if label == 'B':
            command += ['--cfg', 'prepared_upsert']
        with (out / ('compile-' + label + '.log')).open('w') as log:
            subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=True)
        binaries[label] = binary
        manifest['libraries'][label] = {'sha256': sha(library), 'command': command,
                                        'binary_sha256': sha(binary)}
    mother = out / 'mother.sqlite'
    subprocess.run([str(args.seed_cli.resolve()), 'bootstrap', '--json'],
                   env={**os.environ, 'MACEVERY_DB': str(mother)}, check=True, capture_output=True)
    with sqlite3.connect(mother) as conn:
        conn.executemany('INSERT INTO files(path,path_lower,basename,basename_lower,ext_lower,kind,'
                         'size,mtime,ctime,dev,inode,indexed_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)',
                         ((f'/round8/tree-{i % 100}/code-{i:08}.rs',
                           f'/round8/tree-{i % 100}/code-{i:08}.rs', f'code-{i:08}.rs',
                           f'code-{i:08}.rs', 'rs', 'file', i % 4096, 1700000000 + i,
                           1600000000 + i, 1, i + 100, 1) for i in range(args.rows)))
    with sqlite3.connect(mother) as conn:
        conn.execute('PRAGMA wal_checkpoint(TRUNCATE)')
        conn.execute('PRAGMA journal_mode=DELETE')
        assert conn.execute('PRAGMA quick_check').fetchone() == ('ok',)
    manifest['mother_sha256'] = sha(mother)
    (out / 'manifest.local.json').write_text(json.dumps(manifest, indent=2) + '\n')
    results = []
    probe = NativeProbe()
    for ordinal, label in enumerate(['A', 'B', 'A']):
        run = out / f'{ordinal}-{label}'
        run.mkdir(mode=0o700)
        (run / '.macevery-upsert-owned').write_text('macevery-upsert-owned-v1\n')
        database = run / 'index.sqlite'
        copy_mode = clone_snapshot(mother, database)
        assert copy_mode == 'apfs_clone', 'Same-layout gate requires an APFS clone'
        command = [str(binaries[label]), '--owned-root', str(run), '--owned-db', str(database),
                   '--fresh-generation', '2', '--expected-rows', str(args.rows)]
        samples = []
        stop = threading.Event()
        armed = threading.Event()
        started = time.monotonic()
        with (run / 'stderr.txt').open('w') as stderr, (run / 'events.jsonl').open('w') as raw:
            process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=stderr,
                                       start_new_session=True)
            def sample():
                while not stop.is_set():
                    if armed.is_set():
                        try:
                            samples.append({'elapsed_s': time.monotonic() - started,
                                            **probe.usage(process.pid)})
                        except RuntimeError:
                            pass
                    stop.wait(.01)
            sampler = threading.Thread(target=sample)
            sampler.start()
            events = []
            code = None
            try:
                deadline, buffered = started + 120, b''
                fd = process.stdout.fileno()
                os.set_blocking(fd, False)
                while True:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0 or not select.select([fd], [], [], remaining)[0]:
                        raise TimeoutError('Owned upsert driver exceeded 120-second total deadline')
                    chunk = os.read(fd, 65536)
                    if not chunk:
                        assert not buffered, 'Truncated driver JSON'
                        break
                    buffered += chunk
                    assert len(buffered) <= 256 * 1024, 'Oversized driver output'
                    while b'\n' in buffered:
                        line, buffered = buffered.split(b'\n', 1)
                        raw.write(line.decode('utf-8') + '\n')
                        event = json.loads(line)
                        events.append(event)
                        if event['event'] == 'start':
                            samples.append({'elapsed_s': time.monotonic() - started,
                                            **probe.usage(process.pid)})
                            armed.set()
                # EOF does not reap the child. Stop the sampler while this PID
                # remains pinned, then let the unique owner release it.
                stop.set()
                sampler.join()
                code = process.wait(timeout=3)
            finally:
                stop.set()
                sampler.join()
                if code is None:
                    # This owner has not called wait/poll, so PID reuse cannot
                    # redirect cleanup or sampling to a different process.
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    process.wait(timeout=3)
                (run / 'samples.local.json').write_text(json.dumps(samples) + '\n')
        assert code == 0 and events[-1]['event'] == 'complete', run
        assert len(samples) >= 2, 'Insufficient memory samples'
        identities = {(x['start_abstime'], x['uuid']) for x in samples}
        assert len(identities) == 1, 'Process identity changed within memory samples'
        result = {'label': label, 'ordinal': ordinal, 'copy_mode': copy_mode, 'events': events,
                  'sampled_peak_rss_bytes': max((x['rss_bytes'] for x in samples), default=None),
                  'sampled_peak_footprint_bytes': max((x['footprint_bytes'] for x in samples), default=None),
                  'samples': len(samples)}
        results.append(result)
        # Evidence, failures and the small mother remain; obsolete run DBs do not.
        for suffix in ['', '-wal', '-shm']:
            Path(str(database) + suffix).unlink(missing_ok=True)
        (out / 'results.local.json').write_text(json.dumps(results, indent=2) + '\n')
        print(f'{ordinal}-{label}: complete', flush=True)
    assert sha(mother) == manifest['mother_sha256']
    phases = [[e for e in r['events'] if e['event'] == 'phase_end'] for r in results]
    names = ['identical_replay', 'fresh_generation', 'identical_after_generation',
             'sparse_metadata_replay']
    expected_deltas = [0, args.rows, 0, 32]
    for result, phase in zip(results, phases):
        assert [x['phase'] for x in phase] == names, 'Missing or reordered phase'
        assert [x['revision_delta'] for x in phase] == expected_deltas
        assert all(x['complete_ordered_field_equality'] and x['fields_verified_per_record'] == 13
                   and x['rows_before'] == x['rows_after'] == x['attempted_rows'] == args.rows
                   and x['revision_delta_equals_canonical_fixture_expectation'] for x in phase)
        assert all(a['revision_after'] == b['revision_before'] for a, b in zip(phase, phase[1:]))
        if result['label'] == 'B':
            assert [x['api_files_rows_written'] for x in phase] == expected_deltas
    for index in range(4):
        assert all(p[index]['evidence'] == phases[0][index]['evidence'] for p in phases)
    summary = {'status': 'PASS', 'same_ordered_13_fields': True, 'source_unchanged': True,
               'runs': results,
               'notes': ['Transaction CPU/wall exclude preload and verification.',
                         'Sampled memory and lifetime HWM include input Vec and verification.',
                         'WAL/main sizes are not physical IO-byte counters.',
                         'Fresh indexed_at generation remains an observable write in both versions.']}
    (out / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')


if __name__ == '__main__':
    main()
