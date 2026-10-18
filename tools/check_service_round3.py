#!/usr/bin/env python3
"""Sequential A/B HTTP validation of bounded incremental index refresh.

Both binaries must already use the packed memory backend (round-2 baseline vs
round-3 candidate); this script does not compare the old Vec memory backend with
packed storage. Only frozen /tmp snapshots and fresh /tmp output are accepted.
The round-2 tool's reviewed copy/process/metrics helpers are reused without edits.

Each version receives its own copy of the same migrated prepared snapshot. Three
normal transaction waves update existing ordinary files plus two ext:round3 probe
rows. Wave 3 uses 40 simultaneous authenticated HTTP searches with concurrent
health/status probes. Explicit full reload, >4096-revision fallback, journal gap,
missing journal, and a retryable read failure are exercised afterwards. Queries
and committed SQLite probe metadata are compared across both binaries. All RSS
and CPU measurements refer to the service process; controller SQL and preparation
are outside measured phases. Fixed A/B order is warm-cache mechanism evidence,
not a replacement for a repeated A/B/A benchmark.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import json
import os
from pathlib import Path
import secrets
import sqlite3
import threading
import tempfile
import time
from urllib.parse import urlencode

from check_service_round2 import (
    OwnedService, ProcessMetrics, Sampler, clone_or_backup, db_identity, identity,
    nonempty_wal, readonly, request, require, require_tmp, response_json, sha256,
    validate_probes, write_json,
)
import subprocess


ROOT = '/__macevery_round3_service_probe__'
SEARCH = '/search?' + urlencode({'q': 'ext:round3', 'limit': '10'})
NORMAL_WAVES = 3
BURST_REQUESTS = 40


def prepare_probes(database, ordinary_limit):
    with closing(sqlite3.connect(database)) as connection:
        require(connection.execute("SELECT count(*) FROM files WHERE ext_lower='round3'").fetchone()[0] == 0,
                'Frozen snapshot already contains ext:round3 probe rows')
        ordinary = connection.execute(
            "SELECT id,path FROM files WHERE kind='file' ORDER BY id LIMIT ?", (ordinary_limit,)
        ).fetchall()
        require(ordinary, 'Snapshot must contain ordinary file rows')
        for basename, size, modified in [('alpha.round3', 11, 120), ('replace.round3', 22, 130)]:
            path = ROOT + '/' + basename
            connection.execute(
                'INSERT INTO files(path,path_lower,basename,basename_lower,ext_lower,kind,size,mtime,ctime,dev,inode,indexed_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)',
                (path, path.lower(), basename, basename, 'round3', 'file', size, modified, 100, 1, 101, 1700000000000000000),
            )
        connection.commit()
        connection.execute('PRAGMA wal_checkpoint(TRUNCATE)')
    return ordinary


def expected_probes(database):
    with closing(readonly(database)) as connection:
        connection.row_factory = sqlite3.Row
        return {row['path']: dict(row) for row in connection.execute(
            "SELECT path,basename,kind,size,mtime FROM files WHERE ext_lower='round3'")}


def expected_ordinary(database, identifier):
    with closing(readonly(database)) as connection:
        connection.row_factory = sqlite3.Row
        row = connection.execute('SELECT path,basename,kind,size,mtime FROM files WHERE id=?', (identifier,)).fetchone()
        require(row is not None, 'Ordinary probe ID disappeared')
        return dict(row)


def apply_wave(database, wave, ordinary):
    with closing(sqlite3.connect(database)) as connection:
        connection.execute('BEGIN IMMEDIATE')
        connection.executemany(
            'UPDATE files SET size=COALESCE(size,0)+?,mtime=COALESCE(mtime,0)+? WHERE id=?',
            ((wave, wave, identifier) for identifier, _path in ordinary),
        )
        alpha = connection.execute("SELECT id FROM files WHERE ext_lower='round3' AND basename LIKE 'alpha%'").fetchone()[0]
        name = f'alpha-wave-{wave}.round3'
        path = ROOT + '/' + name
        connection.execute('UPDATE files SET path=?,path_lower=?,basename=?,basename_lower=?,size=?,mtime=? WHERE id=?',
                           (path, path.lower(), name, name, 100 + wave, 300 + wave, alpha))
        other = connection.execute("SELECT id FROM files WHERE ext_lower='round3' AND id!=?", (alpha,)).fetchone()[0]
        connection.execute('DELETE FROM files WHERE id=?', (other,))
        name = f'added-wave-{wave}.round3'
        path = ROOT + '/' + name
        # Explicitly reuse the deleted key: journal final state must replace the
        # cached old row, including SQLite DELETE+INSERT within one transaction.
        connection.execute(
            'INSERT INTO files(id,path,path_lower,basename,basename_lower,ext_lower,kind,size,mtime,ctime,dev,inode,indexed_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)',
            (other, path, path.lower(), name, name, 'round3', 'file', 200 + wave, 400 + wave, 100, 1, 102, 1700000000000000000),
        )
        if wave == 2:
            maximum = connection.execute('SELECT MAX(id) FROM files').fetchone()[0]
            connection.execute('UPDATE files SET id=? WHERE id=?', (maximum + 1, other))
        connection.commit()


def update_alpha(database, repeats=1):
    with closing(sqlite3.connect(database)) as connection:
        connection.execute('BEGIN IMMEDIATE')
        for _ in range(repeats):
            connection.execute("UPDATE files SET size=size+1 WHERE ext_lower='round3' AND basename LIKE 'alpha%'")
        connection.commit()


def counters(status):
    value = response_json(status)
    return {key: value[key] for key in ['reload_count', 'reload_attempts', 'incremental_applies', 'incremental_attempts'] if key in value}


def burst(port, token, timeout):
    barrier = threading.Barrier(BURST_REQUESTS + 2)
    stop = threading.Event()

    def search(ordinal):
        barrier.wait(timeout=30)
        value = request(port, SEARCH, token, timeout)
        value['ordinal'] = ordinal
        value['started_monotonic'] = time.monotonic() - value['wall_seconds']
        value['finished_monotonic'] = time.monotonic()
        return value

    def control(path):
        barrier.wait(timeout=30)
        values = []
        while not stop.is_set() or not values:
            started = time.monotonic()
            value = request(port, path, token, timeout)
            value.update(started_monotonic=started, finished_monotonic=time.monotonic())
            values.append(value)
            stop.wait(.01)
        return values

    with ThreadPoolExecutor(max_workers=BURST_REQUESTS + 2) as pool:
        searches = [pool.submit(search, ordinal) for ordinal in range(BURST_REQUESTS)]
        controls = [pool.submit(control, path) for path in ['/health', '/status']]
        try:
            results = [future.result(timeout=timeout + 10) for future in searches]
        finally:
            stop.set()
        probes = [value for future in controls for value in future.result(timeout=timeout + 10)]
    return {'searches': results, 'controls': probes}


def run_version(label, binary, database, ordinary, output, args, metrics):
    output.mkdir()
    from check_service_round2 import free_port
    port, token = free_port(), secrets.token_hex(24)
    environment = {**os.environ, 'MACEVERY_DB': str(database), 'MACEVERY_AUTH_TOKEN': token,
                   'MACEVERY_MAX_ACTIVE_SEARCHES': '2', 'MACEVERY_MAX_ACTIVE_REQUESTS': '64',
                   'MACEVERY_EXIT_WITH_PARENT': '1'}
    command = [str(binary), 'serve', '--addr', f'127.0.0.1:{port}', '--backend', 'memory']
    report = {'label': label, 'binary': identity(binary, True), 'command': command,
              'database': identity(database), 'port': port, 'responses': {}, 'phases': {}}
    service = OwnedService(command, environment, output)
    sampler = Sampler(service.process.pid, args.sample_interval, metrics)

    def measured(name, path, method='GET'):
        value, phase = sampler.measure(name, lambda: request(port, path, token, args.timeout, method))
        report['responses'][name] = value
        report['phases'][name] = phase
        write_json(output / (name + '.response.json'), value)
        return value

    def status(name):
        return measured(name, '/status')

    def search(name):
        value = measured(name, SEARCH)
        validate_probes(value, expected_probes(database))
        return value

    def check_increment(before, after, normal=True, reason=None):
        old, new = counters(before), counters(after)
        if label == 'candidate':
            require(set(new) == {'reload_count', 'reload_attempts', 'incremental_applies', 'incremental_attempts'},
                    'Candidate lacks incremental diagnostics')
            if normal:
                require(new['reload_count'] == old['reload_count'], 'Normal wave performed a candidate full reload')
                require(new['incremental_applies'] == old['incremental_applies'] + 1, 'Normal wave did not publish exactly one candidate delta')
                require(response_json(after)['last_refresh_method'] == 'incremental', 'Normal wave did not report incremental refresh')
            else:
                require(new['reload_count'] == old['reload_count'] + 1, 'Expected exactly one candidate full fallback/reload')
                if reason is not None:
                    require(response_json(after)['last_fallback_reason'] == reason, 'Unexpected full fallback reason')

    try:
        def ready():
            deadline = time.monotonic() + args.timeout
            while time.monotonic() < deadline:
                require(service.reap() is None, 'Service exited before readiness')
                response = request(port, '/health', token, max(.5, deadline - time.monotonic()))
                if response.get('status') == 200:
                    return response
                time.sleep(.05)
            raise TimeoutError('Service load/readiness timed out')

        _ready, phase = sampler.measure('load', ready)
        report['phases']['load'] = phase
        initial_status = status('initial_status')
        search('initial_search')
        previous = initial_status
        for wave in range(1, NORMAL_WAVES + 1):
            apply_wave(database, wave, ordinary)
            if label == 'candidate':
                observed = status(f'wave_{wave}_status_only')
                require(counters(observed) == counters(previous), 'Status triggered refresh')
                require(response_json(observed)['stale'] is True, 'Status did not report the changed snapshot')
            name = f'wave_{wave}_search'
            if wave == NORMAL_WAVES:
                raw, phase = sampler.measure(name, lambda: burst(port, token, args.timeout))
                report['phases'][name] = phase
                report['burst'] = raw
                write_json(output / 'http40.json', raw)
                successful = []
                expected = expected_probes(database)
                for response in raw['searches']:
                    require('error' not in response, f'HTTP40 transport failure: {response}')
                    if response.get('status') == 200:
                        validate_probes(response, expected)
                        successful.append(response)
                    else:
                        require(response.get('status') == 503 and 'busy' in response.get('body', '').lower(), 'HTTP40 did not use explicit backpressure')
                require(successful, 'HTTP40 had no successful query')
                require(all(response['json'] == successful[0]['json'] for response in successful),
                        'HTTP40 successful results differed in fields, score or ordering')
                for endpoint in ['/health', '/status']:
                    samples = [item for item in raw['controls'] if item['path'] == endpoint]
                    require(samples and all(item.get('status') == 200 for item in samples), 'Health/status failed during HTTP40')
                    begin = min(item['started_monotonic'] for item in raw['searches'])
                    end = max(item['finished_monotonic'] for item in raw['searches'])
                    require(any(item['started_monotonic'] < end and item['finished_monotonic'] > begin for item in samples),
                            'Control probes did not overlap the search burst')
                report['http40_status_counts'] = {str(code): sum(item.get('status') == code for item in raw['searches']) for code in [200, 503]}
                report['responses'][name] = successful[0]
            else:
                search(name)
            after = status(f'wave_{wave}_after_status')
            check_increment(previous, after)
            # Exercise a real ordinary row as well as the synthetic extension.
            expected = expected_ordinary(database, ordinary[wave - 1][0])
            path = '/search?' + urlencode({'q': expected['path'], 'path': '1', 'limit': '300'})
            actual = response_json(measured(f'wave_{wave}_ordinary', path))
            matches = [row for row in actual if row['path'] == expected['path']]
            require(len(matches) == 1 and {key: matches[0][key] for key in expected} == expected,
                    'An updated ordinary row was not read/applied correctly')
            previous = after

        explicit = measured('explicit_reload', '/reload', 'POST')
        response_json(explicit)
        search('explicit_after_search')
        after = status('explicit_after_status')
        check_increment(previous, after, normal=False)
        if label == 'candidate':
            require(response_json(after)['last_refresh_method'] == 'explicit_full', 'Explicit reload stopped being full')
        previous = after

        update_alpha(database, repeats=4097)
        search('overflow_search')
        after = status('overflow_after_status')
        check_increment(previous, after, normal=False, reason='event_limit')
        previous = after

        update_alpha(database)
        search('after_overflow_search')
        after = status('after_overflow_status')
        check_increment(previous, after)
        previous = after

        update_alpha(database)
        with closing(sqlite3.connect(database)) as connection:
            connection.execute('DELETE FROM macevery_file_changes')
            connection.commit()
        search('gap_search')
        after = status('gap_after_status')
        check_increment(previous, after, normal=False, reason='history_gap')
        previous = after

        update_alpha(database)
        with closing(sqlite3.connect(database)) as connection:
            connection.execute('DROP TABLE macevery_file_changes')
            connection.commit()
        search('missing_journal_search')
        after = status('missing_journal_after_status')
        check_increment(previous, after, normal=False, reason='journal_unavailable')
        # Legacy baseline cannot repair the journal itself; repair its isolated
        # copy outside measured phases so later fault recovery has valid triggers.
        subprocess.run([str(args.candidate.resolve()), 'status', '--json'], env=environment,
                       capture_output=True, text=True, timeout=args.timeout, check=True)
        previous = after

        update_alpha(database)
        with closing(sqlite3.connect(database)) as connection:
            connection.execute('ALTER TABLE files RENAME TO round3_temporarily_unavailable')
            connection.commit()
        failure = measured('refresh_failure', SEARCH)
        require(failure.get('status') == 503, 'Failed refresh did not return retryable 503')
        failed_status = status('refresh_failure_status')
        require(response_json(failed_status)['ok'] is False, 'Failed refresh claimed to be fresh')
        if label == 'candidate':
            require(counters(failed_status)['incremental_applies'] == counters(previous)['incremental_applies'], 'Failed refresh published an incremental stamp')
        with closing(sqlite3.connect(database)) as connection:
            connection.execute('ALTER TABLE round3_temporarily_unavailable RENAME TO files')
            connection.commit()
        search('refresh_recovery')
        after = status('refresh_recovery_status')
        if label == 'candidate':
            require(response_json(after)['stale'] is False, 'Refresh retry did not recover')
            require(counters(after)['incremental_applies'] == counters(previous)['incremental_applies'] + 1, 'Read-error recovery did not apply the pending delta')
        report['status'] = 'PASS'
    except Exception as error:
        report.update(status='FAIL', error=type(error).__name__ + ': ' + str(error))
    finally:
        sampler.close()
        report['cleanup'] = service.close(port)
        write_json(output / 'process-samples.json', sampler.samples)
        write_json(output / 'summary.json', report)
        require(report['cleanup']['process_group_gone'] and report['cleanup']['port_closed'], 'Owned process/listener remained after cleanup')
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--snapshot', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--ordinary-rows', type=int, default=1000)
    parser.add_argument('--timeout', type=float, default=300)
    parser.add_argument('--sample-interval', type=float, default=.02)
    args = parser.parse_args()
    require(args.timeout > 0 and 3 <= args.ordinary_rows <= 4000 and .001 <= args.sample_interval <= 1, 'Invalid limits')
    args.output = require_tmp(args.output)
    args.output.mkdir(parents=True, exist_ok=True)
    require(not any(args.output.iterdir()), 'Output must be empty')
    report = {'status': 'RUNNING', 'started_utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
              'scope': 'isolated sequential packed-memory A/B HTTP mechanism checks; not production acceptance'}
    try:
        baseline, candidate = args.baseline.resolve(strict=True), args.candidate.resolve(strict=True)
        source = require_tmp(args.snapshot.resolve(strict=True))
        report.update(baseline=identity(baseline, True), candidate=identity(candidate, True),
                      source_before=db_identity(source), snapshot_sha256=sha256(source), ordinary_rows=args.ordinary_rows)
        with tempfile.TemporaryDirectory(prefix='macevery-http-round3-', dir='/private/tmp') as scratch:
            scratch = Path(scratch)
            prepared = scratch / 'prepared' / 'index.sqlite'
            report['snapshot_copy'] = clone_or_backup(source, prepared, args.timeout)
            completed = subprocess.run([str(candidate), 'status', '--json'], env={**os.environ, 'MACEVERY_DB': str(prepared)},
                                       capture_output=True, text=True, timeout=args.timeout, check=True)
            (args.output / 'migration.stdout.json').write_text(completed.stdout)
            (args.output / 'migration.stderr.txt').write_text(completed.stderr)
            ordinary = prepare_probes(prepared, args.ordinary_rows)
            require(len(ordinary) >= NORMAL_WAVES, 'Need at least three ordinary rows')
            require(not nonempty_wal(prepared), 'Prepared snapshot has an uncheckpointed WAL')
            versions = {}
            metrics = ProcessMetrics()
            report['mach_timebase'] = {'numer': metrics.timebase.numer, 'denom': metrics.timebase.denom}
            report['versions'] = versions
            for label, binary in [('baseline', baseline), ('candidate', candidate)]:
                database = scratch / label / 'index.sqlite'
                clone_or_backup(prepared, database, args.timeout)
                versions[label] = run_version(label, binary, database, ordinary, args.output / label, args, metrics)
            require(all(value['status'] == 'PASS' for value in versions.values()), 'One or more service checks failed')
            stages = ['initial_search', 'explicit_after_search', 'overflow_search', 'after_overflow_search',
                      'gap_search', 'missing_journal_search', 'refresh_recovery']
            stages += [f'wave_{wave}_{kind}' for wave in range(1, NORMAL_WAVES + 1) for kind in ['search', 'ordinary']]
            for stage in stages:
                require(versions['baseline']['responses'][stage]['json'] == versions['candidate']['responses'][stage]['json'],
                        f'Ordered baseline/candidate JSON differs at {stage}')
            report['comparison'] = {stage: {label: versions[label]['phases'][stage] for label in ['baseline', 'candidate']}
                                    for stage in ['load', 'wave_1_search', 'wave_2_search', 'wave_3_search', 'explicit_reload', 'overflow_search']}
        report['source_after'] = db_identity(source)
        require(report['source_before'] == report['source_after'], 'Frozen source DB/WAL changed')
        report.update(status='PASS', temporary_copies_removed=True)
    except Exception as error:
        report.update(status='FAIL', error=type(error).__name__ + ': ' + str(error))
    finally:
        report['finished_utc'] = time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())
        write_json(args.output / 'summary.json', report)
    print(json.dumps({'status': report['status'], 'error': report.get('error'), 'summary': str(args.output / 'summary.json')}, ensure_ascii=False))
    return 0 if report['status'] == 'PASS' else 1


if __name__ == '__main__':
    raise SystemExit(main())
