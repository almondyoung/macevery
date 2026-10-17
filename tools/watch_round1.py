#!/usr/bin/env python3
"""Controlled FSEvents A/B, never opens the user's live database."""
import argparse, ctypes, json, os, resource, shutil, sqlite3, subprocess, time
from pathlib import Path


def wait_for(check, label, seconds=20):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(.05)
    raise AssertionError('timed out: ' + label)


def db_rows(db, root):
    with sqlite3.connect('file:' + str(db) + '?mode=ro', uri=True, timeout=2) as c:
        return c.execute('SELECT path,kind,size FROM files WHERE path>=? AND path<? ORDER BY path',
                         (str(root), str(root) + '0')).fetchall()


def run_case(binary, directory, seed_rows):
    directory.mkdir()
    root = directory / 'root'
    root.mkdir()
    # The watcher deliberately ignores the database's whole parent directory.
    # Keep state alongside (never above) the watched root.
    state = directory / 'state'
    state.mkdir()
    db = state / 'index.sqlite'
    env = {**os.environ, 'MACEVERY_DB': str(db)}
    subprocess.run([binary, 'index', '--rebuild', str(root)], env=env, check=True,
                   capture_output=True)
    # Rows outside the watched root exercise cleanup scaling without scanning fake files.
    with sqlite3.connect(db) as c:
        c.executemany('INSERT INTO files(path,path_lower,basename,basename_lower,kind,indexed_at) '
                      'VALUES(?,?,?,?,?,?)',
                      ((f'/round1-unwatched/item-{i:08}.rs', f'/round1-unwatched/item-{i:08}.rs',
                        f'item-{i:08}.rs', f'item-{i:08}.rs', 'file', 1)
                       for i in range(seed_rows)))
        fsevents = ctypes.CDLL('/System/Library/Frameworks/CoreServices.framework/CoreServices')
        fsevents.FSEventsGetCurrentEventId.restype = ctypes.c_uint64
        checkpoint = fsevents.FSEventsGetCurrentEventId()
        c.execute('INSERT OR REPLACE INTO meta(key,value) VALUES(?,?)',
                  ('watch_last_event_id', str(checkpoint)))
    log = open(directory / 'watch.log', 'w')
    proc = subprocess.Popen([binary, 'watch'], env=env, stdout=log, stderr=log)
    started = time.monotonic()
    checks = []
    try:
        wait_for(lambda: 'watching ' in (directory / 'watch.log').read_text(), 'watch ready')
        def rows(): return db_rows(db, root)
        def has(path, size=None):
            return any(p == str(path) and (size is None or s == size) for p,k,s in rows())
        a = root / 'alpha.txt'
        a.write_text('one')
        wait_for(lambda: has(a, 3), 'create'); checks.append('create')
        a.write_text('changed-content')
        wait_for(lambda: has(a, 15), 'modify'); checks.append('modify')
        b = root / 'beta.txt'; a.rename(b)
        wait_for(lambda: has(b, 15) and not has(a), 'rename'); checks.append('rename')
        folder = root / 'folder'; folder.mkdir(); child = folder / 'child.txt'; child.write_text('child')
        wait_for(lambda: has(child, 5), 'directory create'); checks.append('directory create')
        renamed = root / 'folder-renamed'; folder.rename(renamed)
        wait_for(lambda: has(renamed / 'child.txt', 5) and not has(child), 'directory rename')
        checks.append('directory rename')
        shutil.rmtree(renamed)
        wait_for(lambda: not has(renamed / 'child.txt'), 'directory delete'); checks.append('directory delete')
        b.unlink(); wait_for(lambda: not has(b), 'file delete'); checks.append('file delete')
        # Every wave includes directory events under an existing excluded ancestor.
        noise = root / '.git'; noise.mkdir(); time.sleep(.6)
        stress_start = time.monotonic()
        for wave in range(12):
            for i in range(12):
                d = noise / f'wave-{wave}-{i}'; d.mkdir(); (d / 'object').write_text('x')
            time.sleep(.25)
        time.sleep(2)
        assert not any('/.git' in p for p,k,s in rows()), 'excluded paths indexed'
        checks.append('excluded directory write storm')
        # A normal change after the ignored stream proves subsequent updates still work.
        final = root / 'after-noise.txt'; final.write_text('ok')
        wait_for(lambda: has(final, 2), 'update after excluded events')
        checks.append('update after excluded events')
        time.sleep(.5)
        final_rows = [(str(Path(p).relative_to(root)), k, s if k == 'file' else None)
                      for p,k,s in rows()]
        elapsed = time.monotonic() - started
        # Short wait4 accounting includes startup + identical lifecycle + stress + cooldown.
        proc.terminate()
        _, status, usage = os.wait4(proc.pid, 0)
        proc.returncode = os.waitstatus_to_exitcode(status)
        return {'binary': binary, 'rows_seeded': seed_rows, 'elapsed_s': elapsed,
                'cpu_s': usage.ru_utime + usage.ru_stime, 'peak_rss_bytes': usage.ru_maxrss,
                'cpu_percent': 100 * (usage.ru_utime + usage.ru_stime) / elapsed,
                'checks': checks, 'final_rows': final_rows,
                'stress_elapsed_s': time.monotonic() - stress_start,
                'log': str(directory / 'watch.log')}
    finally:
        if proc.returncode is None:
            proc.terminate()
            try: proc.wait(timeout=5)
            except subprocess.TimeoutExpired: proc.kill(); proc.wait()
        log.close()


def main():
    p = argparse.ArgumentParser()
    p.add_argument('--baseline', required=True); p.add_argument('--candidate', required=True)
    p.add_argument('--output', required=True, type=Path); p.add_argument('--rows', type=int, default=300000)
    args = p.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    results = []
    for label, binary in [('baseline',args.baseline), ('candidate',args.candidate), ('baseline-repeat',args.baseline)]:
        print('Running watcher ' + label, flush=True)
        result = run_case(binary, args.output / label, args.rows)
        result['label'] = label; results.append(result)
        (args.output / 'results.json').write_text(json.dumps(results, indent=2)+'\n')
    assert all(x['final_rows'] == results[0]['final_rows'] for x in results), 'A/B index state differs'
    print(json.dumps(results, indent=2))

if __name__ == '__main__': main()
