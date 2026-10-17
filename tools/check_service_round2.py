#!/usr/bin/env python3
"""Measure real sequential HTTP memory-service load/reload on isolated /tmp DBs.

Usage:
  python3 tools/check_service_round2.py --baseline /tmp/old/macevery \
    --candidate /tmp/new/macevery --snapshot /tmp/frozen/index.sqlite \
    --output /tmp/macevery-round2-http

The source must be a frozen /tmp snapshot. It is never opened writable. A
read-only SQLite backup includes committed WAL data; APFS clonefile is used only
when no nonempty WAL exists. The candidate migrates a temporary copy, two fixed
ext:round2 probe rows are seeded there, then each binary receives its own copy.
Services run sequentially on random authenticated loopback ports. Raw responses,
process samples, binary/snapshot SHA256, and cleanup evidence are saved.

macOS proc_pid_rusage reports process-wide CPU Mach ticks, resident bytes and
physical footprint. CPU ticks are converted with mach_timebase_info. A 20 ms RSS sample is a lower bound on a phase's true peak;
wait4 ru_maxrss gives the exact process-lifetime high-water mark in bytes on
macOS. The reload peak budget is a configurable heuristic, not proof of how many
Rust objects exist. CPU excludes this Python controller and fixture preparation.
This script intentionally does not run concurrent-search or fault-injection
suites; those checks are separate evidence.
"""
import argparse
from contextlib import closing
import ctypes
import hashlib
import http.client
import json
import os
from pathlib import Path
import secrets
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from urllib.parse import quote, urlencode


PROBE_ROOT = '/__macevery_round2_service_probe__'
PROBE_FIELDS = ['path', 'basename', 'kind', 'size', 'mtime']
SEARCH = '/search?' + urlencode({'q': 'ext:round2', 'limit': '10'})
RESPONSE_LIMIT = 2 * 1024 * 1024


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def write_json(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n')


def sha256(path):
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        for data in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(data)
    return digest.hexdigest()


def identity(path, include_hash=False):
    stat = path.stat()
    value = {'path': str(path), 'bytes': stat.st_size, 'device': stat.st_dev,
             'inode': stat.st_ino, 'mtime_ns': stat.st_mtime_ns}
    if include_hash:
        value['sha256'] = sha256(path)
    return value


def db_identity(path):
    return {suffix or 'main': identity(Path(str(path) + suffix))
            for suffix in ['', '-wal'] if Path(str(path) + suffix).exists()}


def require_tmp(path):
    absolute = path.resolve()
    require(any(absolute == root or root in absolute.parents
                for root in [Path('/private/tmp'), Path('/tmp').resolve()]),
            f'Only /tmp snapshots/output are accepted: {absolute}')
    return absolute


def readonly(path):
    return sqlite3.connect('file:' + quote(str(path), safe='/') + '?mode=ro',
                           uri=True, timeout=10)


def nonempty_wal(path):
    wal = Path(str(path) + '-wal')
    return wal.exists() and wal.stat().st_size > 0


def clone_or_backup(source, destination, timeout):
    require(not destination.exists(), f'Refusing to overwrite {destination}')
    destination.parent.mkdir(parents=True, exist_ok=True)
    before = db_identity(source)
    method = 'sqlite_readonly_backup'
    if not nonempty_wal(source):
        libc = ctypes.CDLL('/usr/lib/libSystem.B.dylib', use_errno=True)
        clonefile = getattr(libc, 'clonefile', None)
        if clonefile is not None:
            clonefile.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_int]
            clonefile.restype = ctypes.c_int
            if clonefile(os.fsencode(source), os.fsencode(destination), 0) == 0:
                method = 'APFS_clonefile_no_nonempty_WAL'
    if not destination.exists():
        deadline = time.monotonic() + timeout

        def progress(_status, _remaining, _total):
            if time.monotonic() > deadline:
                raise TimeoutError('read-only SQLite backup timed out')

        with closing(readonly(source)) as src, \
                closing(sqlite3.connect(destination)) as dst:
            src.backup(dst, pages=1024, progress=progress, sleep=.01)
            dst.execute('PRAGMA wal_checkpoint(TRUNCATE)')
    require(before == db_identity(source), 'Frozen source DB/WAL changed during copy')
    return {'method': method, 'source': before, 'destination': identity(destination)}


class RusageInfoV0(ctypes.Structure):
    _fields_ = [('uuid', ctypes.c_uint8 * 16)] + [(name, ctypes.c_uint64) for name in [
        'user_time', 'system_time', 'pkg_idle_wkups', 'interrupt_wkups', 'pageins',
        'wired_size', 'resident_size', 'phys_footprint', 'proc_start_abstime',
        'proc_exit_abstime']]


class MachTimebase(ctypes.Structure):
    _fields_ = [('numer', ctypes.c_uint32), ('denom', ctypes.c_uint32)]


class ProcessMetrics:
    def __init__(self):
        require(sys.platform == 'darwin', 'This measurement requires macOS libproc')
        self.library = ctypes.CDLL('/usr/lib/libproc.dylib', use_errno=True)
        self.query = self.library.proc_pid_rusage
        self.query.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
        self.query.restype = ctypes.c_int
        system = ctypes.CDLL('/usr/lib/libSystem.B.dylib')
        system.mach_timebase_info.argtypes = [ctypes.POINTER(MachTimebase)]
        system.mach_timebase_info.restype = ctypes.c_int
        self.timebase = MachTimebase()
        require(system.mach_timebase_info(ctypes.byref(self.timebase)) == 0 and self.timebase.denom,
                'Cannot read Mach CPU timebase')

    def sample(self, pid):
        info = RusageInfoV0()
        if self.query(pid, 0, ctypes.byref(info)) != 0:
            return {'monotonic': time.monotonic(), 'error': os.strerror(ctypes.get_errno())}
        user_ns = info.user_time * self.timebase.numer // self.timebase.denom
        system_ns = info.system_time * self.timebase.numer // self.timebase.denom
        return {'monotonic': time.monotonic(), 'rss_bytes': info.resident_size,
                'physical_footprint_bytes': info.phys_footprint,
                'user_cpu_ticks': info.user_time, 'system_cpu_ticks': info.system_time,
                'user_cpu_ns': user_ns, 'system_cpu_ns': system_ns,
                'cpu_ns': user_ns + system_ns}


class Sampler:
    def __init__(self, pid, interval, metrics):
        self.pid, self.interval, self.metrics = pid, interval, metrics
        self.phase = 'startup'
        self.samples = []
        self.stop_event = threading.Event()
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()

    def capture(self):
        sample = self.metrics.sample(self.pid)
        sample['phase'] = self.phase
        self.samples.append(sample)
        return sample

    def _run(self):
        while not self.stop_event.is_set():
            self.capture()
            self.stop_event.wait(self.interval)

    def measure(self, label, operation):
        self.phase = label
        before = self.capture()
        started = time.monotonic()
        value = operation()
        after = self.capture()
        samples = [item for item in self.samples
                   if item['phase'] == label and 'rss_bytes' in item]
        cpu = (after['cpu_ns'] - before['cpu_ns']) if 'cpu_ns' in before and 'cpu_ns' in after else None
        wall = time.monotonic() - started
        self.phase = 'idle'
        summary = {'wall_seconds': wall, 'cpu_seconds': cpu / 1e9 if cpu is not None else None,
                   'average_cpu_percent_one_core': cpu / 1e9 / wall * 100 if cpu is not None and wall else None,
                   'start_rss_bytes': before.get('rss_bytes'), 'end_rss_bytes': after.get('rss_bytes'),
                   'sampled_peak_rss_bytes': max((item['rss_bytes'] for item in samples), default=None),
                   'samples': len(samples)}
        return value, summary

    def close(self):
        self.stop_event.set()
        self.thread.join(timeout=2)


def request(port, path, token, timeout, method='GET'):
    connection = http.client.HTTPConnection('127.0.0.1', port, timeout=timeout)
    started = time.monotonic()
    result = {'path': path, 'method': method}
    try:
        connection.request(method, path, headers={'Authorization': 'Bearer ' + token})
        response = connection.getresponse()
        body = response.read(RESPONSE_LIMIT + 1)
        require(len(body) <= RESPONSE_LIMIT, 'Response exceeded evidence limit')
        result.update(status=response.status, body=body.decode('utf-8'))
        try:
            result['json'] = json.loads(result['body'])
        except json.JSONDecodeError:
            pass
    except (OSError, http.client.HTTPException) as error:
        result['error'] = type(error).__name__ + ': ' + str(error)
    finally:
        result['wall_seconds'] = time.monotonic() - started
        connection.close()
    return result


def response_json(response):
    require(response.get('status') == 200, f'HTTP request failed: {response}')
    require('json' in response, 'Response was not JSON')
    return response['json']


def status_counters(response):
    status = response_json(response)
    return {key: status[key] for key in ['reload_count', 'reload_attempts'] if key in status}


def free_port():
    with socket.socket() as reserve:
        reserve.bind(('127.0.0.1', 0))
        return reserve.getsockname()[1]


class OwnedService:
    def __init__(self, command, environment, output):
        self.stdout = (output / 'service.stdout.txt').open('w')
        self.stderr = (output / 'service.stderr.txt').open('w')
        self.process = subprocess.Popen(command, env=environment, stdout=self.stdout,
                                        stderr=self.stderr, start_new_session=True)
        self.reaped = None

    def reap(self):
        if self.reaped is None:
            pid, status, usage = os.wait4(self.process.pid, os.WNOHANG)
            if pid:
                self.process.returncode = os.waitstatus_to_exitcode(status)
                self.reaped = {'returncode': self.process.returncode,
                               'lifetime_peak_rss_bytes': usage.ru_maxrss,
                               'user_cpu_seconds': usage.ru_utime,
                               'system_cpu_seconds': usage.ru_stime}
        return self.reaped

    def close(self, port):
        method = 'already_exited'
        if self.reap() is None:
            method = 'SIGTERM'
            try:
                os.killpg(self.process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            deadline = time.monotonic() + 5
            while self.reap() is None and time.monotonic() < deadline:
                time.sleep(.02)
            if self.reap() is None:
                method = 'SIGKILL'
                os.killpg(self.process.pid, signal.SIGKILL)
                while self.reap() is None:
                    time.sleep(.01)
        self.stdout.close()
        self.stderr.close()
        try:
            os.killpg(self.process.pid, 0)
            group_gone = False
        except ProcessLookupError:
            group_gone = True
        with socket.socket() as probe:
            probe.settimeout(.2)
            port_closed = probe.connect_ex(('127.0.0.1', port)) != 0
        return {**self.reaped, 'pid': self.process.pid, 'method': method,
                'process_group_gone': group_gone, 'port_closed': port_closed}


def seed_probes(path):
    with closing(sqlite3.connect(path)) as connection:
        require(connection.execute("SELECT count(*) FROM files WHERE ext_lower='round2'").fetchone()[0] == 0,
                'Snapshot already has ext:round2 rows; choose a clean frozen snapshot')
        for name, size, mtime, inode in [('alpha.round2', 11, 120, 101), ('delete.round2', 22, 130, 102)]:
            value = PROBE_ROOT + '/' + name
            connection.execute('INSERT INTO files(path,path_lower,basename,basename_lower,ext_lower,kind,size,mtime,ctime,dev,inode,indexed_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)',
                               (value, value.lower(), name, name.lower(), 'round2', 'file', size, mtime, 100, 1, inode, 1700000000000000000))
        connection.commit()
        connection.execute('PRAGMA wal_checkpoint(TRUNCATE)')


def expected_probes(path):
    with closing(readonly(path)) as connection:
        connection.row_factory = sqlite3.Row
        return {row['path']: dict(row) for row in connection.execute(
            'SELECT path,basename,kind,size,mtime FROM files WHERE ext_lower=?', ('round2',))}


def validate_probes(response, expected):
    results = response_json(response)
    require(isinstance(results, list), 'Search JSON must be a list')
    require(len(results) == len(expected) == 2, 'Expected exactly two positive probe hits')
    require({row['path'] for row in results} == set(expected), 'Probe paths differ from committed SQLite rows')
    for result in results:
        require({key: result[key] for key in PROBE_FIELDS} == expected[result['path']],
                'Probe metadata differs from committed SQLite rows')
        require(isinstance(result.get('score'), int), 'Probe result lacks integer score')


def mutate_control_metadata(path):
    with closing(sqlite3.connect(path)) as connection:
        connection.execute('BEGIN IMMEDIATE')
        for key, value in [('watch_last_event_id', '424242'), ('last_indexed_at', '424242'),
                           ('round2_metadata_probe', 'changed')]:
            connection.execute('INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value', (key, value))
        connection.execute('INSERT OR IGNORE INTO roots(path) VALUES(?)', (PROBE_ROOT,))
        connection.execute('INSERT OR IGNORE INTO excludes(pattern) VALUES(?)', ('__round2_excluded__',))
        connection.commit()


def mutate_files(path):
    with closing(sqlite3.connect(path)) as connection:
        connection.execute('BEGIN IMMEDIATE')
        changed = PROBE_ROOT + '/alpha-changed.round2'
        cursor = connection.execute('UPDATE files SET path=?,path_lower=?,basename=?,basename_lower=?,size=?,mtime=? WHERE path=?',
                                   (changed, changed.lower(), 'alpha-changed.round2', 'alpha-changed.round2', 99, 300, PROBE_ROOT + '/alpha.round2'))
        require(cursor.rowcount == 1, 'Expected one updated probe')
        cursor = connection.execute('DELETE FROM files WHERE path=?', (PROBE_ROOT + '/delete.round2',))
        require(cursor.rowcount == 1, 'Expected one deleted probe')
        name = 'added.round2'
        added = PROBE_ROOT + '/' + name
        connection.execute('INSERT INTO files(path,path_lower,basename,basename_lower,ext_lower,kind,size,mtime,ctime,dev,inode,indexed_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)',
                           (added, added.lower(), name, name, 'round2', 'file', 33, 400, 100, 1, 103, 1700000000000000000))
        connection.commit()


def run_version(label, binary, database, output, args, metrics):
    output.mkdir()
    token, port = secrets.token_hex(24), free_port()
    environment = {**os.environ, 'MACEVERY_DB': str(database), 'MACEVERY_AUTH_TOKEN': token,
                   'MACEVERY_MAX_ACTIVE_SEARCHES': '1', 'MACEVERY_MAX_ACTIVE_REQUESTS': '8',
                   'MACEVERY_EXIT_WITH_PARENT': '1'}
    command = [str(binary), 'serve', '--addr', f'127.0.0.1:{port}', '--backend', 'memory']
    report = {'label': label, 'binary': identity(binary, True), 'command': command,
              'database': identity(database), 'port': port, 'phases': {}, 'responses': {}}
    service = OwnedService(command, environment, output)
    sampler = Sampler(service.process.pid, args.sample_interval, metrics)
    responses, phases = report['responses'], report['phases']

    def measured(name, path, method='GET'):
        response, phase = sampler.measure(name, lambda: request(port, path, token, args.timeout, method))
        responses[name], phases[name] = response, phase
        write_json(output / (name + '.response.json'), response)
        return response

    try:
        def await_ready():
            deadline = time.monotonic() + args.timeout
            while time.monotonic() < deadline:
                require(service.reap() is None, 'Owned service exited before readiness')
                response = request(port, '/health', token, max(.5, deadline - time.monotonic()))
                if response.get('status') == 200:
                    return response
                time.sleep(.05)
            raise TimeoutError('Service load/readiness timed out')

        responses['load_health'], phases['load'] = sampler.measure('load', await_ready)
        initial_status = measured('initial_status', '/status')
        initial = measured('initial_search', SEARCH)
        validate_probes(initial, expected_probes(database))
        counts = status_counters(initial_status)
        report['legacy_status_before_search_skipped'] = label == 'baseline'
        if label == 'candidate':
            require(set(counts) == {'reload_count', 'reload_attempts'}, 'Candidate lacks reload counters')
        mutate_control_metadata(database)
        # The legacy /status actively reloads. Do not let it consume the
        # metadata-change cost before the baseline's measured /search.
        metadata_status = measured('metadata_status_only', '/status') if label == 'candidate' else None
        metadata_search = measured('metadata_search', SEARCH)
        metadata_after = measured('metadata_after_status', '/status')
        require(response_json(metadata_search) == response_json(initial), 'Metadata-only writes changed ordered search JSON')
        if label == 'candidate':
            require(status_counters(metadata_status) == counts == status_counters(metadata_after),
                    'Metadata-only writes or status triggered a candidate reload')
            require(response_json(metadata_status).get('stale') is False, 'Metadata-only writes made candidate stale')
        mutate_files(database)
        changed_status = measured('files_status_only', '/status') if label == 'candidate' else None
        if label == 'candidate':
            require(status_counters(changed_status) == counts, '/status actively loaded changed candidate content')
            require(response_json(changed_status).get('stale') is True, '/status failed to observe stale candidate content')
        changed = measured('files_search_reload', SEARCH)
        validate_probes(changed, expected_probes(database))
        require(response_json(changed) != response_json(initial), 'External add/change/delete did not update search')
        changed_after = measured('files_after_status', '/status')
        if label == 'candidate':
            new_counts = {key: value + 1 for key, value in counts.items()}
            require(status_counters(changed_after) == new_counts, 'File mutation must cause exactly one candidate reload')
        explicit = measured('explicit_reload', '/reload', 'POST')
        response_json(explicit)
        after_reload = measured('explicit_after_search', SEARCH)
        validate_probes(after_reload, expected_probes(database))
        require(response_json(after_reload) == response_json(changed), 'Explicit reload changed ordered search results')
        final_status = measured('explicit_after_status', '/status')
        if label == 'candidate':
            require(status_counters(final_status) == {key: value + 2 for key, value in counts.items()},
                    'Explicit reload must cause exactly one additional candidate load')
        steady = max(phases['explicit_reload']['start_rss_bytes'], phases['explicit_reload']['end_rss_bytes'])
        budget = int(steady * args.reload_peak_ratio + 64 * 1024 * 1024)
        peak = phases['explicit_reload']['sampled_peak_rss_bytes']
        report['reload_peak_budget'] = {'sampled_peak_rss_bytes': peak, 'steady_rss_bytes': steady,
                                        'ratio_limit': args.reload_peak_ratio, 'fixed_allowance_bytes': 64 * 1024 * 1024,
                                        'budget_bytes': budget, 'within_budget': peak <= budget,
                                        'interpretation': 'sampled phase budget; lifetime exact high-water is in cleanup'}
        if label == 'candidate':
            require(peak <= budget, 'Candidate explicit reload exceeded configured single-index RSS budget')
        report['status'] = 'PASS'
    except Exception as error:
        report['status'] = 'FAIL'
        report['error'] = type(error).__name__ + ': ' + str(error)
    finally:
        sampler.close()
        report['cleanup'] = service.close(port)
        write_json(output / 'process-samples.json', sampler.samples)
        require(report['cleanup']['process_group_gone'] and report['cleanup']['port_closed'],
                'Owned service or listener survived cleanup')
        write_json(output / 'summary.json', report)
    return report


def run(args, report):
    require(sys.platform == 'darwin', 'macOS is required for this script')
    baseline, candidate = [path.resolve(strict=True) for path in [args.baseline, args.candidate]]
    source = require_tmp(args.snapshot.resolve(strict=True))
    require(source.is_file(), 'Snapshot must be an existing SQLite file')
    report.update(baseline=identity(baseline, True), candidate=identity(candidate, True),
                  source_before=db_identity(source), snapshot_sha256=sha256(source),
                  metrics={'rss': 'proc_pid_rusage RUSAGE_INFO_V0 resident_size bytes',
                           'cpu': 'proc_pid_rusage user_time+system_time Mach ticks converted using mach_timebase_info; whole process including threads',
                           'lifetime_peak': 'wait4 ru_maxrss bytes on Darwin',
                           'phase_peak': f'sampled every {args.sample_interval}s; lower bound',
                           'cpu_percent': '100% = one fully occupied CPU core; fixture/controller CPU excluded'})
    with tempfile.TemporaryDirectory(prefix='macevery-http-round2-', dir='/private/tmp') as scratch:
        scratch = Path(scratch)
        prepared = scratch / 'prepared' / 'index.sqlite'
        report['snapshot_copy'] = clone_or_backup(source, prepared, args.timeout)
        migration = subprocess.run([str(candidate), 'status', '--json'],
                                   env={**os.environ, 'MACEVERY_DB': str(prepared)},
                                   capture_output=True, text=True, timeout=args.timeout)
        (args.output / 'migration.stdout.json').write_text(migration.stdout)
        (args.output / 'migration.stderr.txt').write_text(migration.stderr)
        require(migration.returncode == 0, 'Candidate failed to migrate temporary snapshot copy')
        seed_probes(prepared)
        require(not nonempty_wal(prepared), 'Prepared copy has nonempty WAL before cloning')
        report['prepared_probes'] = expected_probes(prepared)
        report['prepared_records'] = sum(json.loads(migration.stdout).get(key, 0)
                                         for key in ['files', 'dirs', 'apps', 'symlinks', 'others']) + 2
        report['copies'] = {}
        versions = {}
        metrics = ProcessMetrics()
        report['metrics']['mach_timebase'] = {'numer': metrics.timebase.numer, 'denom': metrics.timebase.denom,
                                              'tick_nanoseconds': metrics.timebase.numer / metrics.timebase.denom}
        for label, binary in [('baseline', baseline), ('candidate', candidate)]:
            database = scratch / label / 'index.sqlite'
            report['copies'][label] = clone_or_backup(prepared, database, args.timeout)
            versions[label] = run_version(label, binary, database, args.output / label, args, metrics)
        report['versions'] = versions
        require(all(item['status'] == 'PASS' for item in versions.values()), 'One or more HTTP service checks failed')
        for stage in ['initial_search', 'metadata_search', 'files_search_reload', 'explicit_after_search']:
            require(versions['baseline']['responses'][stage]['json'] == versions['candidate']['responses'][stage]['json'],
                    f'Baseline/candidate ordered JSON differ at {stage}')
        report['comparison'] = {stage: {label: versions[label]['phases'][stage]
                                       for label in ['baseline', 'candidate']}
                                for stage in ['load', 'metadata_search', 'files_search_reload', 'explicit_reload']}
    report['source_after'] = db_identity(source)
    require(report['source_before'] == report['source_after'], 'Frozen source DB/WAL changed during checks')
    report['temporary_copies_removed'] = True
    report['status'] = 'PASS'


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--snapshot', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--timeout', type=float, default=300)
    parser.add_argument('--sample-interval', type=float, default=.02)
    parser.add_argument('--reload-peak-ratio', type=float, default=1.6)
    args = parser.parse_args()
    require(args.timeout > 0 and .001 <= args.sample_interval <= 1 and args.reload_peak_ratio >= 1,
            'Invalid timeout/sample interval/reload ratio')
    args.output = require_tmp(args.output)
    args.output.mkdir(parents=True, exist_ok=True)
    require(not any(args.output.iterdir()), 'Output must be an empty directory to preserve prior evidence')
    report = {'status': 'RUNNING', 'started_utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())}
    try:
        run(args, report)
    except Exception as error:
        report['status'] = 'FAIL'
        report['error'] = type(error).__name__ + ': ' + str(error)
    finally:
        report['finished_utc'] = time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())
        write_json(args.output / 'summary.json', report)
    print(json.dumps({'status': report['status'], 'summary': str(args.output / 'summary.json'),
                      'error': report.get('error')}, ensure_ascii=False))
    return 0 if report['status'] == 'PASS' else 1


if __name__ == '__main__':
    raise SystemExit(main())
