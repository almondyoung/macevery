#!/usr/bin/env python3
"""Exercise the real HTTP service against an isolated SQLite backup.

Example (starts one temporary loopback listener; never starts the GUI/watcher):
  python3 tools/check_service_round1.py --binary /tmp/new/release/macevery \
    --baseline /tmp/base/release/macevery --db /tmp/synthetic/index.sqlite \
    --output /tmp/macevery-http-evidence

The source is opened read-only for SQLite backup, including committed WAL data.
Only temporary copies are passed to MacEvery. Forty synchronized HTTP searches
must return the baseline CLI's ordered JSON or explicit 503 backpressure. Health
and authenticated status are probed concurrently. Evidence contains full response
JSON, timings, binary hashes, source identity, and owned-process cleanup checks.
CLI candidate limits can differ from an in-memory full scan for broad queries;
this check reports such a mismatch rather than treating it as an optimization PASS.
"""
import argparse
import concurrent.futures
from contextlib import closing
import hashlib
import http.client
import json
import os
from pathlib import Path
import secrets
import shutil
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from urllib.parse import quote, urlencode


BURST_REQUESTS = 40
RESPONSE_LIMIT = 16 * 1024 * 1024


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def write_json(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n')


def file_identity(path):
    stat = path.stat()
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(chunk)
    return {'path': str(path), 'bytes': stat.st_size, 'mtime_ns': stat.st_mtime_ns,
            'inode': stat.st_ino, 'sha256': digest.hexdigest()}


def source_identity(path):
    return {name: file_identity(item) for name, item in
            [('main', path), ('wal', Path(str(path) + '-wal'))] if item.exists()}


def readonly(path):
    return sqlite3.connect('file:' + quote(str(path), safe='/') + '?mode=ro',
                           uri=True, timeout=5)


def backup_snapshot(source, destination):
    deadline = time.monotonic() + 30

    def progress(_status, _remaining, _total):
        if time.monotonic() > deadline:
            raise TimeoutError('SQLite backup exceeded 30 seconds')

    with closing(readonly(source)) as src, closing(sqlite3.connect(destination)) as dst:
        src.backup(dst, pages=256, progress=progress, sleep=.01)
        dst.execute('PRAGMA wal_checkpoint(TRUNCATE)')


def json_digest(value):
    data = json.dumps(value, sort_keys=True, ensure_ascii=False,
                      separators=(',', ':')).encode()
    return hashlib.sha256(data).hexdigest()


def request(port, path, token=None, timeout=15):
    started = time.monotonic()
    result = {'path': path, 'started': started}
    connection = http.client.HTTPConnection('127.0.0.1', port, timeout=timeout)
    try:
        headers = {'Authorization': 'Bearer ' + token} if token else {}
        connection.request('GET', path, headers=headers)
        response = connection.getresponse()
        body = response.read(RESPONSE_LIMIT + 1)
        require(len(body) <= RESPONSE_LIMIT, 'HTTP response exceeded evidence limit')
        result.update(status=response.status, body=body.decode('utf-8'))
        if response.status == 200:
            result['json'] = json.loads(result['body'])
    except Exception as error:
        result['error'] = type(error).__name__ + ': ' + str(error)
    finally:
        connection.close()
        result['finished'] = time.monotonic()
        result['wall_ms'] = (result['finished'] - started) * 1000
    return result


def search_path(query, limit):
    return '/search?' + urlencode({'q': query, 'limit': str(limit)})


def free_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as reserve:
        reserve.bind(('127.0.0.1', 0))
        return reserve.getsockname()[1]


def stop_owned_process(process, port):
    method = 'already_exited'
    if process.poll() is None:
        method = 'SIGTERM'
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            method = 'SIGKILL'
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)
    else:
        process.wait()
    try:
        os.killpg(process.pid, 0)
        group_gone = False
    except ProcessLookupError:
        group_gone = True
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.settimeout(.2)
        port_closed = probe.connect_ex(('127.0.0.1', port)) != 0
    return {'pid': process.pid, 'returncode': process.returncode, 'method': method,
            'process_group_gone': group_gone, 'port_closed': port_closed}


def baseline_results(binary, db, queries, limit, output, environment):
    expected = {}
    for ordinal, query in enumerate(queries):
        command = [str(binary), 'search', query, '--limit', str(limit), '--json']
        completed = subprocess.run(command, env={**environment, 'MACEVERY_DB': str(db)},
                                   capture_output=True, text=True, timeout=30)
        (output / f'baseline-{ordinal:02d}.stdout.json').write_text(completed.stdout)
        (output / f'baseline-{ordinal:02d}.stderr.txt').write_text(completed.stderr)
        require(completed.returncode == 0, f'Baseline CLI failed for {query!r}')
        expected[query] = json.loads(completed.stdout)
        require(isinstance(expected[query], list), 'Baseline CLI returned non-list JSON')
    return expected


def validate_search(response, expected):
    require('error' not in response, f'HTTP transport failed: {response.get("error")}')
    if response['status'] == 200:
        require(response.get('json') == expected, 'Ordered HTTP JSON differs from baseline CLI')
        response['result_count'] = len(expected)
        response['result_sha256'] = json_digest(expected)
    else:
        require(response['status'] == 503, f'Unexpected search status: {response["status"]}')
        require('busy' in response['body'].lower(), '503 did not explicitly report backpressure')


def concurrent_burst(port, token, path):
    barrier = threading.Barrier(BURST_REQUESTS + 2)
    done = threading.Event()

    def search_one(ordinal):
        barrier.wait(timeout=15)
        response = request(port, path, token)
        response['ordinal'] = ordinal
        return response

    def controls(endpoint):
        barrier.wait(timeout=15)
        samples = []
        while not done.is_set() or not samples:
            samples.append(request(port, endpoint, token))
            done.wait(.002)
        return samples

    with concurrent.futures.ThreadPoolExecutor(max_workers=BURST_REQUESTS + 2) as pool:
        searches = [pool.submit(search_one, i) for i in range(BURST_REQUESTS)]
        probes = [pool.submit(controls, endpoint) for endpoint in ['/health', '/status']]
        try:
            results = [future.result(timeout=30) for future in searches]
        finally:
            done.set()
        controls_result = [sample for future in probes for sample in future.result(timeout=30)]
    return results, controls_result


def run(args, report):
    binary, baseline, source = [path.resolve(strict=True) for path in
                                [args.binary, args.baseline, args.db]]
    require(binary.is_file() and baseline.is_file(), 'Binary arguments must be files')
    report.update(binary=file_identity(binary), baseline=file_identity(baseline),
                  source_before=source_identity(source), backend=args.backend,
                  burst_requests=BURST_REQUESTS, search_slots=2, limit=args.limit)
    temp_parent = '/private/tmp' if Path('/private/tmp').is_dir() else '/tmp'
    with tempfile.TemporaryDirectory(prefix='macevery-http-round1-', dir=temp_parent) as scratch:
        scratch = Path(scratch)
        snapshot = scratch / 'snapshot.sqlite'
        backup_snapshot(source, snapshot)
        report['source_after_backup'] = source_identity(source)
        require(report['source_before'] == report['source_after_backup'],
                'Source DB/WAL changed during backup; use a frozen synthetic source')
        with closing(readonly(snapshot)) as connection:
            rows = connection.execute('SELECT COUNT(*) FROM files').fetchone()[0]
            first = connection.execute("SELECT basename FROM files WHERE kind='file' ORDER BY id LIMIT 1").fetchone()
        require(rows > 0 and first, 'Synthetic source must contain at least one file')
        report['snapshot'] = {'rows': rows, 'sha256': file_identity(snapshot)['sha256']}
        queries = list(dict.fromkeys((args.query or [first[0], '~rpt']) + [args.burst_query]))
        report['queries'] = queries
        candidate_db, baseline_db = scratch / 'candidate.sqlite', scratch / 'baseline.sqlite'
        shutil.copyfile(snapshot, candidate_db)
        shutil.copyfile(snapshot, baseline_db)
        token = secrets.token_hex(24)
        environment = {**os.environ, 'MACEVERY_AUTH_TOKEN': token,
                       'MACEVERY_MAX_ACTIVE_SEARCHES': '2', 'MACEVERY_MAX_ACTIVE_REQUESTS': '64',
                       'MACEVERY_EXIT_WITH_PARENT': '1'}
        expected = baseline_results(baseline, baseline_db, queries, args.limit, args.output, environment)
        require(any(expected[query] for query in queries), 'No positive-result query was exercised')
        report['baseline_queries'] = {query: {'count': len(value), 'sha256': json_digest(value)}
                                      for query, value in expected.items()}
        port = free_port()
        command = [str(binary), 'serve', '--addr', f'127.0.0.1:{port}', '--backend', args.backend]
        report.update(port=port, command=command)
        with (args.output / 'service.stdout.txt').open('w') as stdout, \
                (args.output / 'service.stderr.txt').open('w') as stderr:
            process = subprocess.Popen(command, env={**environment, 'MACEVERY_DB': str(candidate_db)},
                                       stdout=stdout, stderr=stderr, start_new_session=True)
            report['service_pid'] = process.pid
            try:
                deadline = time.monotonic() + 30
                while True:
                    require(process.poll() is None, 'Service exited before readiness')
                    ready = request(port, '/health', timeout=1)
                    if ready.get('status') == 200:
                        break
                    require(time.monotonic() < deadline, 'Service readiness timed out')
                    time.sleep(.05)
                auth_checks = [
                    request(port, '/health'), request(port, '/status'),
                    request(port, search_path(args.burst_query, args.limit)),
                    request(port, '/status', 'invalid-token'), request(port, '/status', token),
                ]
                report['auth_checks'] = auth_checks
                require([item.get('status') for item in auth_checks] == [200, 401, 401, 401, 200],
                        'Health/auth boundary differs from expected statuses')
                require(auth_checks[-1]['json'].get('records') == rows, 'Service loaded a different row count')
                serial = [request(port, search_path(query, args.limit), token) for query in queries]
                report['serial_searches'] = serial
                for query, response in zip(queries, serial):
                    validate_search(response, expected[query])
                    require(response['status'] == 200, 'Idle serial request unexpectedly backpressured')
                results, controls_result = concurrent_burst(
                    port, token, search_path(args.burst_query, args.limit))
                report['burst'] = results
                report['concurrent_controls'] = controls_result
                # Save all raw responses in the report even when validation fails.
                for response in results:
                    validate_search(response, expected[args.burst_query])
                begin = min(item['started'] for item in results)
                end = max(item['finished'] for item in results)
                for endpoint in ['/health', '/status']:
                    samples = [item for item in controls_result if item['path'] == endpoint]
                    require(samples and all(item.get('status') == 200 and 'error' not in item for item in samples),
                            f'{endpoint} failed during concurrent searches')
                    require(any(item['started'] < end and item['finished'] > begin for item in samples),
                            f'{endpoint} was not probed during the search burst')
                statuses = {str(status): sum(item['status'] == status for item in results) for status in [200, 503]}
                report['burst_status_counts'] = statuses
                require(statuses['200'] > 0, 'No search completed successfully')
                # A tiny fixture can complete before threads overlap inside the scorer.
                # Keep this separate from a proven backpressure observation.
                report['backpressure_observed'] = statuses['503'] > 0
                recovery = request(port, search_path(args.burst_query, args.limit), token)
                report['recovery'] = recovery
                validate_search(recovery, expected[args.burst_query])
                require(recovery['status'] == 200, 'Search slots did not recover after the burst')
            finally:
                report['cleanup'] = stop_owned_process(process, port)
        require(report['cleanup']['process_group_gone'] and report['cleanup']['port_closed'],
                'Owned service process group or listening port remains after cleanup')
    report['source_after'] = source_identity(source)
    require(report['source_before'] == report['source_after'], 'Source DB/WAL changed during the check')
    report['temporary_db_copies_removed'] = True
    report['status'] = 'PASS' if report['backpressure_observed'] else 'PASS_WITH_BACKPRESSURE_NOT_EXERCISED'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True, type=Path)
    parser.add_argument('--baseline', required=True, type=Path)
    parser.add_argument('--db', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--backend', choices=['memory', 'compact', 'sqlite', 'auto'], default='memory')
    parser.add_argument('--limit', type=int, default=300)
    parser.add_argument('--query', action='append', help='Repeat to override the default serial queries')
    parser.add_argument('--burst-query', default='~rpt')
    args = parser.parse_args()
    require(args.limit >= 0, '--limit must be nonnegative')
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    report = {'status': 'RUNNING', 'started_unix': time.time(),
              'scope': 'isolated local HTTP + baseline CLI; no GUI or live-index acceptance'}
    code = 0
    try:
        run(args, report)
    except Exception as error:
        report.update(status='FAIL', error=type(error).__name__ + ': ' + str(error))
        code = 1
    finally:
        report['finished_unix'] = time.time()
        write_json(args.output / 'report.json', report)
    print(json.dumps({key: report.get(key) for key in
                      ['status', 'error', 'burst_status_counts', 'backpressure_observed', 'cleanup']},
                     ensure_ascii=False, indent=2))
    print('Evidence: ' + str(args.output / 'report.json'))
    return code


if __name__ == '__main__':
    sys.exit(main())
