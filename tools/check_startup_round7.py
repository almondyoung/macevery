#!/usr/bin/env python3
"""Isolated status->serve versus bootstrap->serve->ready->full-status A/B/A.

Example (root schedules performance work sequentially):
  python3 tools/check_startup_round7.py --baseline /tmp/base/macevery \
    --candidate /tmp/new/macevery --snapshot /tmp/frozen/index.sqlite \
    --rounds 2 --order aba --query code --limit 300 --expect-count 300 \
    --output /tmp/macevery-startup-round7

No GUI, watcher, installed App or live database is used. A candidate status CLI
prepares/migrates one temporary copy and supplies the exact counts/config oracle
outside timing. Each run gets its own clone of that frozen prepared copy.
Baseline runs status --json then serve --backend memory. Candidate runs bootstrap
--json then starts the memory service. Only after strict readiness is sampled does
it start an owned full status CLI, followed immediately by the first search. This
matches the GUI's deferred-count startup without measuring a window or watcher.

Startup CPU at readiness/first query includes bootstrap plus the full status
CLI's CPU consumed up to that milestone plus service CPU. Lifetime CPU includes
the complete full status task even if it finishes after the first query. Status
completion wall time is reported separately. No background work is omitted.
wait4 owns CLI/lifetime CPU and per-process RSS HWM. Live milestone CPU/RSS and
footprint use the round2 libproc/Mach-timebase helper. Concurrent process HWMs
are separate values and must not be added and called a simultaneous RSS peak.
Sampling can miss short peaks; caches/scheduling remain uncontrolled.

Only summary.json is suitable for publication: it has counts, hashes, numeric
metrics and relative raw references, with no token or real result/config paths.
manifest.local.json and raw/ retain local paths, complete ordered responses,
samples, failures, and owned-process cleanup receipts. No failed run is dropped.
"""
import argparse
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import secrets
import sqlite3
import sys
import tempfile
import threading
import time
from urllib.parse import urlencode

from check_service_round1 import json_digest
from check_service_round2 import (
    OwnedService, ProcessMetrics, Sampler, clone_or_backup, db_identity, free_port,
    identity, nonempty_wal, request, require, require_tmp, response_json, sha256,
    write_json,
)
from check_startup_round6 import HTTP_FIELDS, close_owned, contains_time_filter, status_cli, summarize


SCOPE = 'isolated status then serve versus bootstrap then serve then strict-ready deferred full-status; no GUI/window/watcher'
COUNT_KEYS = ['files', 'dirs', 'apps', 'symlinks', 'others']
BOOTSTRAP_KEYS = {'db_path', 'has_records', 'roots', 'excludes', 'last_indexed_at'}


def status_counts(value):
    require(isinstance(value, dict), 'Full status must be an object')
    counts = {key: value[key] for key in COUNT_KEYS}
    require(all(type(v) is int and v >= 0 for v in counts.values()), 'Invalid full status counts')
    return counts


def configuration(value):
    require(isinstance(value, dict), 'Configuration must be an object')
    for key in ['roots', 'excludes']:
        require(isinstance(value[key], list) and all(isinstance(s, str) for s in value[key]),
                'Invalid configuration array')
    last = value['last_indexed_at']
    require(last is None or (type(last) is int and -(1 << 63) <= last < (1 << 63)),
            'Invalid last indexed time')
    return {key: value[key] for key in ['roots', 'excludes', 'last_indexed_at']}


def validate_status(value, expected, database):
    require(value.get('db_path') == str(database), 'Full status database path differs')
    require(status_counts(value) == status_counts(expected), 'Full status counts differ from prepared oracle')
    require(configuration(value) == configuration(expected), 'Full status configuration differs from prepared oracle')


def validate_bootstrap(value, expected, database):
    require(isinstance(value, dict) and set(value) == BOOTSTRAP_KEYS, 'Unexpected bootstrap JSON schema')
    require(value['db_path'] == str(database), 'Bootstrap database path differs')
    require(type(value['has_records']) is bool, 'Bootstrap has_records must be boolean')
    require(value['has_records'] == (sum(status_counts(expected).values()) > 0),
            'Bootstrap record-existence differs from exact oracle')
    require(configuration(value) == configuration(expected), 'Bootstrap configuration differs from prepared oracle')


class CliTask:
    """One wait4 owner; synchronized live samples never race another reap()."""
    def __init__(self, binary, command, database, directory, args, metrics):
        self.args, self.metrics, self.directory = args, metrics, directory
        self.lock, self.cancel = threading.Lock(), threading.Event()
        self.started = time.monotonic()
        self.raw = {'command': [str(binary), command, '--json'], 'started_monotonic': self.started}
        self.child = OwnedService(self.raw['command'], {**os.environ, 'MACEVERY_DB': str(database)}, directory)
        self.sampler = None
        self.thread = None
        try:
            self.sampler = Sampler(self.child.process.pid, args.sample_interval, metrics)
            self.thread = threading.Thread(target=self._run, name=f'macevery-{command}-wait4', daemon=True)
            self.thread.start()
        except BaseException:
            if self.sampler is not None:
                self.sampler.close()
            self.raw['cleanup'] = close_owned(self.child, 0)
            raise

    def _run(self):
        try:
            while True:
                with self.lock:
                    done = self._reap_locked()
                if done is not None:
                    break
                require(not self.cancel.is_set(), 'Owned CLI cancelled')
                require(time.monotonic() - self.started < self.args.timeout, 'Owned CLI timed out')
                self.cancel.wait(self.args.sample_interval)
        except BaseException as error:
            self.raw['failure'] = repr(error)
        finally:
            try:
                self.sampler.close()
                self.raw['samples'] = self.sampler.samples
                with self.lock:
                    self.raw['cleanup'] = close_owned(self.child, 0)
                    clean = self.raw['cleanup']
                    self.raw['cpu_ms'] = 1000 * (clean['user_cpu_seconds'] + clean['system_cpu_seconds'])
                require(clean['process_group_gone'], 'Owned CLI process group remained')
                require(clean['returncode'] == 0, 'Owned CLI failed')
                stdout = self.directory / 'service.stdout.txt'
                require(stdout.stat().st_size <= 8 * 1024 * 1024, 'Owned CLI output too large')
                self.raw['json'] = json.loads(stdout.read_text())
            except BaseException as error:
                self.raw.setdefault('failure', repr(error))

    def _reap_locked(self):
        done = self.child.reap()
        if done is not None and 'completed_monotonic' not in self.raw:
            self.raw['completed_monotonic'] = time.monotonic()
            self.raw['wall_ms'] = (self.raw['completed_monotonic'] - self.started) * 1000
        return done

    def cpu_at_milestone(self):
        with self.lock:
            # An exited child may not yet have been observed by the worker.
            # Reaping under this same lock preserves its exact wait4 receipt.
            final = self._reap_locked()
            if final is not None:
                return {'cpu_ms': 1000 * (final['user_cpu_seconds'] + final['system_cpu_seconds']),
                        'source': 'wait4_completed', 'sample_monotonic': time.monotonic()}
            sample = self.metrics.sample(self.child.process.pid)
            if 'cpu_ns' not in sample:
                final = self._reap_locked()
                require(final is not None, 'Cannot sample running full status CPU')
                return {'cpu_ms': 1000 * (final['user_cpu_seconds'] + final['system_cpu_seconds']),
                        'source': 'wait4_completed_after_sample', 'sample_monotonic': time.monotonic()}
            return {'cpu_ms': sample['cpu_ns'] / 1e6, 'source': 'libproc_running',
                    'sample_monotonic': sample['monotonic'], 'sample': sample}

    def wait(self):
        remaining = max(0, self.args.timeout - (time.monotonic() - self.started))
        self.thread.join(timeout=remaining + 12)
        require(not self.thread.is_alive(), 'Owned CLI cleanup thread did not finish')
        require('failure' not in self.raw, 'Owned CLI failed; see local raw receipt')
        return self.raw['json']

    def close(self):
        self.cancel.set()
        self.thread.join(timeout=12)
        require(not self.thread.is_alive(), 'Owned CLI process cleanup did not finish')
        return self.raw


def cli_metrics(raw, prefix):
    valid = [s for s in raw.get('samples', []) if 'rss_bytes' in s]
    latest = valid[-1] if valid else {}
    clean = raw.get('cleanup', {})
    return {
        prefix + '_wall_ms': raw.get('wall_ms'),
        prefix + '_cpu_ms': raw.get('cpu_ms'),
        prefix + '_lifetime_peak_rss_bytes': clean.get('lifetime_peak_rss_bytes'),
        prefix + '_last_sample_rss_bytes': latest.get('rss_bytes'),
        prefix + '_last_sample_footprint_bytes': latest.get('physical_footprint_bytes'),
        prefix + '_sampled_peak_rss_bytes': max((s['rss_bytes'] for s in valid), default=None),
        prefix + '_sampled_peak_footprint_bytes': max((s['physical_footprint_bytes'] for s in valid), default=None),
    }


def cleanup_public(raw):
    clean = raw.get('cleanup')
    return ({key: clean[key] for key in ['pid', 'method', 'returncode', 'process_group_gone']}
            if clean else None)


def run_one(binary, variant, block, ordinal, database, directory, expected, args, metrics):
    run_id = f'run-{ordinal:02d}-{variant}'
    result = {'run_id': run_id, 'variant': variant, 'block': block, 'status': 'FAIL', 'metrics': {}}
    raw = {'scope': SCOPE, 'database': str(database), 'query': args.query}
    bootstrap = status = service = sampler = None
    port = None
    for part in ['bootstrap', 'status', 'serve']:
        (directory / part).mkdir(parents=True)
    started = time.monotonic()
    try:
        if variant == 'candidate':
            bootstrap = CliTask(binary, 'bootstrap', database, directory / 'bootstrap', args, metrics)
            validate_bootstrap(bootstrap.wait(), expected, database)
            result['bootstrap_contract_match'] = True
            gate = bootstrap
        else:
            status = CliTask(binary, 'status', database, directory / 'status', args, metrics)
            validate_status(status.wait(), expected, database)
            gate = status
        gate_finished = time.monotonic()
        result['metrics'].update(gate_cli_wall_ms=(gate_finished - started) * 1000,
                                 gate_cli_cpu_ms=gate.raw['cpu_ms'])
        counts = status_counts(expected)
        total = sum(counts.values())
        require(total > 0, 'Nonempty snapshot required for this startup comparison')
        result.update(records=total, counts=counts)

        port, token = free_port(), secrets.token_hex(24)
        raw['serve_command'] = [str(binary), 'serve', '--addr', f'127.0.0.1:{port}', '--backend', 'memory']
        environment = {**os.environ, 'MACEVERY_DB': str(database), 'MACEVERY_AUTH_TOKEN': token,
                       'MACEVERY_EXIT_WITH_PARENT': '1'}
        serve_started = time.monotonic()
        raw['serve_started_monotonic'] = serve_started
        service = OwnedService(raw['serve_command'], environment, directory / 'serve')
        sampler = Sampler(service.process.pid, args.sample_interval, metrics)
        raw['readiness_attempts'] = []
        deadline = serve_started + args.timeout
        while True:
            require(service.reap() is None, 'Service exited before readiness')
            remaining = deadline - time.monotonic()
            require(remaining > 0, 'Strict service readiness timed out')
            response = request(port, '/status', token, remaining)
            raw['readiness_attempts'].append(response)
            if response.get('status') == 200:
                ready = response_json(response)
                require('ready' in ready and 'stale' in ready, 'Service lacks explicit readiness fields')
                if ready['ready'] is True and ready['stale'] is False:
                    require(ready['backend'] == 'memory', 'Fastest preference changed')
                    require(type(ready['records']) is int and ready['records'] == total,
                            'Ready records differ from prepared exact count oracle')
                    break
            require(response.get('status') != 401, 'Service token rejected')
            time.sleep(min(args.sample_interval, max(0, deadline - time.monotonic())))
        ready_at = time.monotonic()
        ready_sample = sampler.capture()
        require('cpu_ns' in ready_sample, 'Cannot sample ready service')
        require(status is not None or (variant == 'candidate' and bootstrap is not None),
                'Only candidate deferred status may be unstarted at readiness')
        raw['status_cpu_at_ready'] = (status.cpu_at_milestone() if status is not None else
                                      {'cpu_ms': 0, 'source': 'not_started', 'sample_monotonic': ready_at})
        bootstrap_cpu = bootstrap.raw['cpu_ms'] if bootstrap is not None else 0
        result['metrics'].update({
            'serve_ready_wall_ms': (ready_at - serve_started) * 1000,
            'serve_ready_cpu_ms': ready_sample['cpu_ns'] / 1e6,
            'serve_ready_rss_bytes': ready_sample['rss_bytes'],
            'serve_ready_footprint_bytes': ready_sample['physical_footprint_bytes'],
            'status_cpu_at_ready_ms': raw['status_cpu_at_ready']['cpu_ms'],
            'bootstrap_ready_wall_ms': (ready_at - started) * 1000,
            'bootstrap_ready_cpu_ms': bootstrap_cpu + raw['status_cpu_at_ready']['cpu_ms'] + ready_sample['cpu_ns'] / 1e6,
        })
        if variant == 'candidate':
            status = CliTask(binary, 'status', database, directory / 'status', args, metrics)
        require(status is not None, 'Full status must start before the first search')
        raw['full_status_started_monotonic'] = status.started
        query_started = time.monotonic()
        raw['query_started_monotonic'] = query_started
        raw['first_search'] = request(port, '/search?' + urlencode({'q': args.query, 'limit': args.limit}), token, args.timeout)
        query_finished = time.monotonic()
        query_sample = sampler.capture()
        require('cpu_ns' in query_sample, 'Cannot sample first-search service')
        raw['status_cpu_at_first_search'] = status.cpu_at_milestone()
        rows = response_json(raw['first_search'])
        require(isinstance(rows, list) and len(rows) <= args.limit, 'Invalid first search result')
        require(all(isinstance(row, dict) and set(row) == HTTP_FIELDS for row in rows), 'Unexpected HTTP result fields')
        if args.expect_count is not None:
            require(len(rows) == args.expect_count, 'First search count differs from --expect-count')
        result.update(result_count=len(rows), ordered_json_sha256=json_digest(rows))
        result['metrics'].update({
            'first_search_wall_ms': (query_finished - query_started) * 1000,
            'first_search_cpu_ms': (query_sample['cpu_ns'] - ready_sample['cpu_ns']) / 1e6,
            'serve_first_search_cpu_ms': query_sample['cpu_ns'] / 1e6,
            'serve_first_search_wall_ms': (query_finished - serve_started) * 1000,
            'serve_first_search_rss_bytes': query_sample['rss_bytes'],
            'serve_first_search_footprint_bytes': query_sample['physical_footprint_bytes'],
            'status_cpu_at_first_search_ms': raw['status_cpu_at_first_search']['cpu_ms'],
            'bootstrap_first_search_wall_ms': (query_finished - started) * 1000,
            'bootstrap_first_search_cpu_ms': bootstrap_cpu + raw['status_cpu_at_first_search']['cpu_ms'] + query_sample['cpu_ns'] / 1e6,
        })
        validate_status(status.wait(), expected, database)
        result['full_status_matches_oracle'] = True
        result['metrics']['full_status_complete_wall_ms'] = (status.raw['completed_monotonic'] - started) * 1000
        result['metrics']['all_startup_work_complete_wall_ms'] = (max(query_finished, status.raw['completed_monotonic']) - started) * 1000
        result['status'] = 'PASS'
    except Exception as error:
        result['error_type'] = type(error).__name__
        raw['failure'] = repr(error)
    finally:
        for name, task in [('bootstrap', bootstrap), ('status', status)]:
            if task is None:
                continue
            raw[name + '_cli'] = task.raw
            try:
                task.close()
                result[name + '_cleanup'] = cleanup_public(task.raw)
                result['metrics'].update(cli_metrics(task.raw, 'bootstrap_cli' if name == 'bootstrap' else 'status'))
                require(task.raw.get('cleanup', {}).get('process_group_gone'), 'Owned CLI group remained')
                require(task.raw['cleanup']['returncode'] == 0, 'Owned CLI failed')
            except Exception as error:
                result.update(status='FAIL', error_type=type(error).__name__)
                raw[name + '_cleanup_failure'] = repr(error)
        try:
            if sampler is not None:
                sampler.close()
                raw['serve_samples'] = sampler.samples
            if service is not None:
                raw['serve_cleanup'] = close_owned(service, port)
                clean = raw['serve_cleanup']
                result['cleanup'] = {key: clean[key] for key in
                                     ['pid', 'method', 'returncode', 'process_group_gone', 'port_closed']}
                require(clean['process_group_gone'] and clean['port_closed'], 'Owned service process or port remained')
                require(clean['returncode'] == -15, 'Service did not survive through normal shutdown')
                result['metrics']['serve_lifetime_peak_rss_bytes'] = clean['lifetime_peak_rss_bytes']
                result['metrics']['serve_lifetime_cpu_ms'] = 1000 * (clean['user_cpu_seconds'] + clean['system_cpu_seconds'])
                valid = [s for s in raw['serve_samples'] if 'rss_bytes' in s]
                result['metrics']['serve_sampled_peak_rss_bytes'] = max((s['rss_bytes'] for s in valid), default=None)
                result['metrics']['serve_sampled_peak_footprint_bytes'] = max((s['physical_footprint_bytes'] for s in valid), default=None)
                if status is not None and 'cpu_ms' in status.raw and (bootstrap is None or 'cpu_ms' in bootstrap.raw):
                    result['metrics']['bootstrap_lifetime_cpu_ms'] = (
                        (bootstrap.raw['cpu_ms'] if bootstrap else 0) + status.raw['cpu_ms']
                        + result['metrics']['serve_lifetime_cpu_ms'])
        except Exception as error:
            result.update(status='FAIL', error_type=type(error).__name__)
            raw['cleanup_failure'] = repr(error)
        write_json(directory / 'raw.local.json', raw)
    result['raw_reference'] = f'raw/{run_id}/raw.local.json'
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    for option in ['baseline', 'candidate', 'snapshot', 'output']:
        parser.add_argument('--' + option, type=Path, required=True)
    parser.add_argument('--rounds', type=int, default=2, help='Number of independent AB/ABA blocks')
    parser.add_argument('--order', choices=['ab', 'aba'], default='aba')
    parser.add_argument('--query', default='code')
    parser.add_argument('--limit', type=int, default=300)
    parser.add_argument('--expect-count', type=int, default=300, help='Exact first-query count assertion (default: 300)')
    parser.add_argument('--timeout', type=float, default=180, help='Seconds per CLI/readiness/query stage')
    parser.add_argument('--sample-interval', type=float, default=.01)
    args = parser.parse_args()
    if (sys.platform != 'darwin' or args.rounds < 1 or args.limit < 1 or args.timeout <= 0
            or not .001 <= args.sample_interval <= 1 or not args.query.strip()
            or (args.expect_count is not None and not 0 <= args.expect_count <= args.limit)
            or contains_time_filter(args.query)):
        parser.error('Requires macOS, valid positive limits and a query without relative-time filters')
    source = require_tmp(args.snapshot.resolve(strict=True))
    require(not nonempty_wal(source), 'Freeze the snapshot with online backup before measuring')
    journal = Path(str(source) + '-journal')
    require(not journal.exists() or journal.stat().st_size == 0, 'Snapshot has an active rollback journal')
    output = require_tmp(args.output)
    require(not output.exists(), 'Output must be fresh; earlier evidence is preserved')
    output.mkdir(mode=0o700, parents=True)
    (output / 'raw').mkdir(mode=0o700)
    binaries = {key: getattr(args, key).resolve(strict=True) for key in ['baseline', 'candidate']}
    original = db_identity(source)
    manifest = {'scope': SCOPE, 'arguments': {key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items()},
                'binaries': {key: identity(value, True) for key, value in binaries.items()}, 'source_before': original,
                'snapshot_sha256': sha256(source)}
    helpers = {name: sha256(Path(__file__).with_name(name)) for name in
               ['check_startup_round7.py', 'check_startup_round6.py', 'check_service_round1.py',
                'check_service_round2.py', 'compare_round1.py']}
    runs, report = [], {'status': 'FAIL', 'scope': SCOPE, 'helper_sha256': helpers,
                       'snapshot_sha256': manifest['snapshot_sha256'],
                       'binary_sha256': {key: value['sha256'] for key, value in manifest['binaries'].items()},
                       'query_sha256': hashlib.sha256(args.query.encode()).hexdigest(), 'limit': args.limit,
                       'rounds': args.rounds, 'order': args.order, 'runs': [],
                       'full_status_start_policy': 'candidate_after_strict_ready_before_first_search',
                       'notes': [
                           'Baseline: full status then serve. Candidate: bootstrap then serve, strict ready, then full status plus first search.',
                           'No GUI/window, watcher, preferences, fixed live port or filesystem scans are used.',
                           'A candidate full-status oracle and all copies/migrations are outside timing.',
                           'bootstrap_* metric names denote the complete startup flow, not just the new bootstrap CLI.',
                           'Milestone CPU sums bootstrap, full-status CPU consumed at that point, and service CPU.',
                           'Lifecycle CPU includes the complete background full-status task and service shutdown.',
                           'Full-status completion wall time is separate; moving work off the gate does not remove it.',
                           'Live process CPU samples follow the milestone by small controller/sampling delays.',
                           'CLI completion times are first wait4 observations, with sample-interval polling plus scheduling delay.',
                           'Per-process lifetime RSS HWMs are separate and are never added as a concurrent peak.',
                           'Last/sample RSS and footprint can miss short peaks and differ from storage capacity.',
                           'All runs/failures remain; caches, scheduling and phase overlap are uncontrolled.',
                           'HTTP hashes cover all returned fields/order, not hidden FileRecord fields.',
                           'Only summary.json is suitable for publication; manifest/raw contain private paths/results.',
                       ]}
    scratch_name = None
    try:
        metrics = ProcessMetrics()
        manifest['mach_timebase'] = {'numer': metrics.timebase.numer, 'denom': metrics.timebase.denom}
        with tempfile.TemporaryDirectory(prefix='macevery-startup-round7-', dir='/private/tmp') as scratch_name:
            scratch = Path(scratch_name)
            prepared = scratch / 'prepared' / 'index.sqlite'
            manifest['preparation_copy'] = clone_or_backup(source, prepared, args.timeout)
            prep_dir = output / 'raw' / 'preparation-status'
            prep_dir.mkdir()
            prep = {}
            try:
                expected = status_cli(binaries['candidate'], prepared, prep_dir, args, metrics, prep)
                oracle_counts = status_counts(expected)
                configuration(expected)
                require(sum(oracle_counts.values()) > 0, 'Nonempty prepared snapshot required')
            finally:
                write_json(prep_dir / 'raw.local.json', prep)
            report['oracle_counts'] = oracle_counts
            report['oracle_records'] = sum(oracle_counts.values())
            with closing(sqlite3.connect(prepared)) as connection:
                connection.execute('PRAGMA wal_checkpoint(TRUNCATE)')
            require(not nonempty_wal(prepared), 'Prepared snapshot has uncheckpointed WAL')
            manifest['prepared_sha256'] = sha256(prepared)
            ordinal = 0
            for block in range(args.rounds):
                for variant in (['baseline', 'candidate', 'baseline'] if args.order == 'aba' else ['baseline', 'candidate']):
                    ordinal += 1
                    run_id = f'run-{ordinal:02d}-{variant}'
                    database = scratch / run_id / 'index.sqlite'
                    copied = clone_or_backup(prepared, database, args.timeout)
                    directory = output / 'raw' / run_id
                    directory.mkdir()
                    write_json(directory / 'copy.local.json', copied)
                    runs.append(run_one(binaries[variant], variant, block, ordinal, database,
                                        directory, expected, args, metrics))
            require(sha256(prepared) == manifest['prepared_sha256'], 'Prepared shared input changed')
        report['temporary_copies_removed'] = True
        require(all(run['status'] == 'PASS' for run in runs), 'One or more startup runs failed; all receipts retained')
        require(len({r['ordered_json_sha256'] for r in runs}) == 1, 'Ordered HTTP digests differ across runs')
        require(len({json.dumps(r['counts'], sort_keys=True) for r in runs}) == 1, 'Status counts differ across runs')
        require(len({r['records'] for r in runs}) == 1 and len({r['result_count'] for r in runs}) == 1, 'Run counts differ')
        require(original == db_identity(source) and sha256(source) == manifest['snapshot_sha256'], 'Frozen source changed')
        require(all(identity(path, True) == manifest['binaries'][key] for key, path in binaries.items()), 'Binary changed during measurement')
        report.update(status='PASS', source_unchanged=True, ordered_digest_match=True,
                      status_counts_match=True, ordered_json_sha256=runs[0]['ordered_json_sha256'])
    except Exception as error:
        report['error_type'] = type(error).__name__
        (output / 'driver-failure.local.txt').write_text(repr(error) + '\n')
    finally:
        if scratch_name is not None:
            report['temporary_copies_removed'] = not Path(scratch_name).exists()
        report['runs'] = runs
        report.update(summarize(runs, args.order))
        write_json(output / 'manifest.local.json', manifest)
        write_json(output / 'summary.json', report)
    print(json.dumps({'status': report['status'], 'completed_runs': len(runs),
                      'ordered_digest_match': report.get('ordered_digest_match', False)}))
    return 0 if report['status'] == 'PASS' else 1


if __name__ == '__main__':
    raise SystemExit(main())
