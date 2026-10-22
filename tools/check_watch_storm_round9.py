#!/usr/bin/env python3
"""Sequential real FSEvents A/B/A on one owned tree and fresh DB clones.

Checks excluded traffic, exclusion rename/recreation, ordinary bursts and
updates after a burst. Does not claim to inject or guarantee OS UserDropped.
The same physical tree/path is reused after each owned process has exited.
All retained state is under a fresh /private/tmp output; never the live index.
"""
import argparse
import ctypes
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import time

from check_service_round2 import ProcessMetrics, sha256


def wait_for(check, label, seconds=30):
    stop = time.monotonic() + seconds
    while time.monotonic() < stop:
        if check():
            return
        time.sleep(0.05)
    raise AssertionError('timed out: ' + label)


def rows(db, root):
    with sqlite3.connect('file:' + str(db) + '?mode=ro', uri=True, timeout=2) as c:
        return c.execute('SELECT path,kind,size FROM files WHERE path>=? AND path<? ORDER BY path',
                         (str(root), str(root) + '0')).fetchall()


def run(args):
    out = args.output.resolve()
    if not str(out).startswith('/private/tmp/') or out.exists():
        raise ValueError('output must be a new owned /private/tmp directory')
    out.mkdir(parents=True)
    (out / '.macevery-round9-owned').write_text('round9-watch-storm-v1\n')
    tree = out / 'tree'
    tree.mkdir()
    (tree / 'Library').mkdir()
    mother = out / 'mother.sqlite'
    subprocess.run([str(args.baseline), 'index', '--rebuild', '--exclude', 'Library', str(tree)],
                   env={**os.environ, 'MACEVERY_DB': str(mother)}, check=True, capture_output=True)
    fs = ctypes.CDLL('/System/Library/Frameworks/CoreServices.framework/CoreServices')
    fs.FSEventsGetCurrentEventId.restype = ctypes.c_uint64
    metrics = ProcessMetrics()
    results = []
    for run_id, (label, binary) in enumerate([('baseline', args.baseline), ('candidate', args.candidate), ('baseline-repeat', args.baseline)]):
        # Reset only the exact tree created above, after the previous watcher
        # exited. There are no symlinks created by this harness.
        for child in tree.iterdir():
            if child.is_symlink():
                raise ValueError('unexpected symlink in owned tree')
            shutil.rmtree(child) if child.is_dir() else child.unlink()
        (tree / 'Library').mkdir()
        state = out / f'run-{run_id}'
        state.mkdir()
        db = state / 'index.sqlite'
        subprocess.run(['/bin/cp', '-c', str(mother), str(db)], check=True)
        with sqlite3.connect(db) as c:
            c.execute('INSERT OR REPLACE INTO meta(key,value) VALUES(?,?)',
                      ('watch_last_event_id', str(fs.FSEventsGetCurrentEventId())))
        log_path = state / 'watch.log'
        checks = []
        with log_path.open('w') as log:
            proc = subprocess.Popen([str(binary), 'watch'], env={**os.environ, 'MACEVERY_DB': str(db)}, stdout=log, stderr=log)
            try:
                wait_for(lambda: 'watching ' in log_path.read_text(), 'watch ready')
                before = metrics.sample(proc.pid)
                started = time.monotonic()
                def has(path, size=None):
                    return any(p == str(path) and (size is None or s == size) for p, _, s in rows(db, tree))
                library = tree / 'Library'
                for i in range(args.noise_files):
                    (library / f'noise-{i:05}.txt').write_text('excluded-content')
                marker = tree / 'after-noise-code.txt'
                marker.write_text('one')
                wait_for(lambda: has(marker, 3), 'normal update after excluded storm')
                assert not any('/Library/' in p or p == str(library) for p, _, _ in rows(db, tree))
                checks.append('excluded storm and following normal update')
                # An excluded directory moved into the indexed namespace must
                # bring every child back; moving it out must remove all rows.
                included = tree / 'included'
                library.rename(included)
                wait_for(lambda: has(included / f'noise-{args.noise_files-1:05}.txt', 16), 'excluded-to-included rename')
                actual = [p for p, _, _ in rows(db, tree) if p.startswith(str(included) + '/')]
                assert len(actual) == args.noise_files
                checks.append('excluded-to-included rename complete subtree')
                included.rename(library)
                wait_for(lambda: not any(p == str(included) or p.startswith(str(included) + '/') for p, _, _ in rows(db, tree)), 'included-to-excluded rename')
                checks.append('included-to-excluded rename cleanup')
                shutil.rmtree(library)
                library.mkdir()
                (library / 'recreated.txt').write_text('excluded')
                marker.write_text('modified')
                wait_for(lambda: has(marker, 8), 'excluded directory recreation then update')
                assert not any('/Library/' in p for p, _, _ in rows(db, tree))
                checks.append('excluded deletion/recreation')
                burst = tree / 'burst'
                burst.mkdir()
                for i in range(args.burst_files):
                    (burst / f'code-{i:05}.txt').write_text(str(i))
                wait_for(lambda: len([p for p, _, _ in rows(db, tree) if p.startswith(str(burst) + '/')]) == args.burst_files, 'ordinary burst complete')
                for i in range(args.burst_files):
                    assert has(burst / f'code-{i:05}.txt', len(str(i)))
                checks.append('ordinary burst all sizes and paths')
                marker.rename(tree / 'final-code.txt')
                wait_for(lambda: has(tree / 'final-code.txt', 8) and not has(marker), 'rename after burst')
                checks.append('rename after burst')
                time.sleep(0.5)
                after = metrics.sample(proc.pid)
                wall = time.monotonic() - started
                final = [(str(Path(p).relative_to(tree)), k, s if k == 'file' else None) for p, k, s in rows(db, tree)]
                proc.terminate()
                _, status, usage = os.wait4(proc.pid, 0)
                proc.returncode = os.waitstatus_to_exitcode(status)
                trigger = state / 'macevery-watch-trigger.jsonl'
                event_records = []
                for path in [trigger.with_suffix('.jsonl.1'), trigger]:
                    if path.exists():
                        event_records.extend(json.loads(line) for line in path.read_text().splitlines())
                result = {'label': label, 'binary_sha256': sha256(binary), 'checks': checks, 'wall_seconds': wall,
                          'operation_cpu_seconds': (after['cpu_ns']-before['cpu_ns'])/1e9,
                          'lifetime_cpu_seconds': usage.ru_utime+usage.ru_stime, 'lifetime_hwm_rss_bytes': usage.ru_maxrss,
                          'point_before': before, 'point_after': after, 'final_rows': final,
                          'visible_received': sum(r.get('received', 0) for r in event_records),
                          'visible_rejected': sum(r.get('rejected', 0) for r in event_records),
                          'stream_configuration': [r for r in event_records if r.get('kind') == 'stream_configuration'],
                          'notes': ['CPU includes waits and all operations; point memory is not peak footprint.', 'Bounded logs may suppress/rotate; visible counts are not total OS event counts.']}
                if label == 'candidate':
                    assert any(r.get('upstream_requested', 0) > 0 and r.get('upstream_accepted')
                               for r in result['stream_configuration']), 'upstream exclusions were not active'
                results.append(result)
                (out / 'results.local.json').write_text(json.dumps(results, indent=2)+'\n')
                print(json.dumps({k:v for k,v in result.items() if k not in ['final_rows','stream_configuration']}), flush=True)
            finally:
                if proc.poll() is None:
                    proc.terminate()
                    try:
                        proc.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        proc.kill(); proc.wait()
        assert all(r['final_rows'] == results[0]['final_rows'] for r in results)
    return 0


if __name__ == '__main__':
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--baseline', type=Path, required=True)
    p.add_argument('--candidate', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--noise-files', type=int, default=3000)
    p.add_argument('--burst-files', type=int, default=200)
    a = p.parse_args()
    if not 1 <= a.noise_files <= 10000 or not 1 <= a.burst_files <= 1000:
        p.error('bounded file counts required')
    a.baseline=a.baseline.resolve();a.candidate=a.candidate.resolve()
    raise SystemExit(run(a))
