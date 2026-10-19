#!/usr/bin/env python3
"""Exercise early HTTP requests using one frozen CLI and isolated /tmp clones.

  python3 tools/check_startup_early.py --binary /path/to/frozen/macevery \
    --snapshot /tmp/frozen/index.sqlite --output /tmp/macevery-early-http

Four fresh services run sequentially: reference (query after strict ready), hold
code while loading, close/clear its connection, and rapid supersession of three
connections. Each service uses a random authenticated loopback port, memory
backend, and its own database clone. No GUI, watcher, full-count background task,
installed App mutation, power action, or live database is involved.

This is BACKEND-ONLY evidence. Closing a socket is not a GUI query-clear test;
there is no Python imitation of GUI result publication or generation guards.
The server may still compute a request after its client closes. A TCP listener
is bound before synchronous loading completes, so early requests can wait with
no HTTP response; no response must never be reported as an observed 503.

All observed 200 result arrays must match the reference service's warmed HTTP
oracle in all six fields and order. The reference scenario CPU ends before its
extra oracle preparation. Per-process lifetime CPU/HWM includes all actual work,
including those extra reference requests, so unequal lifetime workloads are not
a performance A/B. CPU snapshots are cumulative for the whole service, not CPU
attributable to a particular concurrent request. The GUI's 15-second threshold
is reported, not enforced by this backend client and not claimed as a UI test.

summary.json has hashes/counts/metrics and relative raw references only. Private
manifest/raw files retain paths, queries, complete responses, and samples. The
default requires an early send before the load receipt plus >=100ms until a
load-completion receipt; tiny smoke may use --allow-unobserved-loading
--expect-count N. A cancelled request need not itself wait for that receipt.
Copies/oracle preparation/controller CPU are excluded from scenario metrics.
"""
import argparse
from contextlib import closing
import hashlib
import http.client
import json
import math
import os
from pathlib import Path
import secrets
import socket
import sqlite3
import sys
import tempfile
import threading
import time
from urllib.parse import urlencode

from check_service_round1 import json_digest
from check_service_round2 import (
    OwnedService, ProcessMetrics, Sampler, clone_or_backup, db_identity, free_port,
    identity, nonempty_wal, require, require_tmp, sha256, write_json,
)
from check_startup_round6 import HTTP_FIELDS, close_owned, contains_time_filter, status_cli


SCOPE = 'isolated frozen-binary early HTTP backend checks; GUI publication NOT_RUN'
RESPONSE_LIMIT = 2 * 1024 * 1024
BUSY_RETRY_DELAYS = [.15, .3, .6, 1.2, 1.2]


def digest_text(value):
    return hashlib.sha256(value.encode()).hexdigest()


class HttpTask:
    """One connection with explicit send, cancellation and completion receipts."""
    def __init__(self, label, port, target, token, timeout, metrics, service_pid, load_log):
        self.raw = {'label': label, 'target': target, 'started_monotonic': time.monotonic()}
        self.port, self.target, self.token = port, target, token
        self.deadline = self.raw['started_monotonic'] + timeout
        self.metrics, self.service_pid, self.load_log = metrics, service_pid, load_log
        self.lock = threading.Lock()
        self.cancelled, self.sent = threading.Event(), threading.Event()
        self.sock = None
        self.thread = threading.Thread(target=self._run, name='early-http-' + label, daemon=True)
        self.thread.start()

    def _sample(self, name):
        try:
            self.raw[name] = self.metrics.sample(self.service_pid)
        except Exception as error:
            self.raw[name] = {'monotonic': time.monotonic(), 'error_type': type(error).__name__}

    def _run(self):
        response = sock = None
        try:
            attempts = 0
            while sock is None:
                require(not self.cancelled.is_set(), 'Request cancelled before connection')
                remaining = self.deadline - time.monotonic()
                require(remaining > 0, 'TCP connection deadline expired')
                attempts += 1
                try:
                    sock = socket.create_connection(('127.0.0.1', self.port), timeout=min(.25, remaining))
                except OSError:
                    if time.monotonic() >= self.deadline:
                        raise
                    self.cancelled.wait(.01)
            self.raw['connection_attempts'] = attempts
            self.raw['connected_monotonic'] = time.monotonic()
            with self.lock:
                self.sock = sock
                require(not self.cancelled.is_set(), 'Request cancelled before send')
            sock.settimeout(max(.001, self.deadline - time.monotonic()))
            header = (f'GET {self.target} HTTP/1.1\r\nHost: 127.0.0.1:{self.port}\r\n'
                      f'Authorization: Bearer {self.token}\r\nConnection: close\r\n\r\n')
            self.raw['send_started_monotonic'] = time.monotonic()
            sock.sendall(header.encode('ascii'))
            self.raw['sent_monotonic'] = time.monotonic()
            self._sample('service_at_send')
            self.raw['sent_before_loaded_log'] = b'serve: loaded ' not in self.load_log.read_bytes()
            self.sent.set()
            response = http.client.HTTPResponse(sock)
            response.begin()
            self.raw['headers_monotonic'] = time.monotonic()
            self.raw['status'] = response.status
            body = response.read(RESPONSE_LIMIT + 1)
            require(len(body) <= RESPONSE_LIMIT, 'HTTP response exceeded evidence limit')
            self.raw['body'] = body.decode('utf-8')
            try:
                self.raw['json'] = json.loads(self.raw['body'])
            except json.JSONDecodeError:
                pass
        except Exception as error:
            self.raw['error_type'] = type(error).__name__
            self.raw['error'] = repr(error)
        finally:
            self.raw['finished_monotonic'] = time.monotonic()
            self._sample('service_at_finish')
            if 'sent_monotonic' in self.raw:
                elapsed = self.raw['finished_monotonic'] - self.raw['sent_monotonic']
                self.raw['sent_to_finish_ms'] = elapsed * 1000
                self.raw['crossed_gui_15s'] = elapsed >= 15
            if response is not None:
                response.close()
            with self.lock:
                self.sock = None
            if sock is not None:
                sock.close()

    def wait_sent(self):
        while not self.sent.wait(.01):
            require(self.thread.is_alive(), 'HTTP request failed before send; see raw receipt')
            require(time.monotonic() < self.deadline, 'HTTP send timed out')

    def cancel(self):
        self.raw.setdefault('cancel_requested_monotonic', time.monotonic())
        self.cancelled.set()
        with self.lock:
            sock = self.sock
            if sock is not None:
                try:
                    sock.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
                sock.close()

    def wait(self):
        self.thread.join(timeout=max(0, self.deadline - time.monotonic()) + 2)
        if self.thread.is_alive():
            self.cancel()
            self.thread.join(timeout=2)
        require(not self.thread.is_alive(), 'HTTP worker remained after deadline')
        return self.raw

    def close(self):
        if self.thread.is_alive():
            self.cancel()
        self.thread.join(timeout=3)
        require(not self.thread.is_alive(), 'HTTP worker remained during cleanup')


def rows_from(response, limit):
    require(response.get('status') == 200 and 'error' not in response,
            'Expected complete HTTP 200; absence of a response is not 503')
    rows = response.get('json')
    require(isinstance(rows, list) and len(rows) <= limit, 'Invalid HTTP result array')
    require(all(isinstance(row, dict) and set(row) == HTTP_FIELDS for row in rows),
            'Unexpected HTTP result fields')
    return rows


def request_public(raw):
    value = {key: raw[key] for key in [
        'label', 'status', 'error_type', 'connection_attempts', 'sent_before_loaded_log',
        'sent_to_finish_ms', 'crossed_gui_15s',
    ] if key in raw}
    value['cancel_requested'] = 'cancel_requested_monotonic' in raw
    value['observed_503'] = raw.get('status') == 503
    value['complete_200'] = raw.get('status') == 200 and 'error' not in raw
    if value['complete_200'] and isinstance(raw.get('json'), list):
        value['count'] = len(raw['json'])
        value['ordered_json_sha256'] = json_digest(raw['json'])
    return value


def run_case(name, binary, database, directory, args, metrics, expected_total, oracle):
    result = {'case': name, 'status': 'FAIL', 'scope': SCOPE, 'metrics': {},
              'gui_publish_validation': 'NOT_RUN'}
    raw = {'case': name, 'database': str(database), 'queries': [args.query, args.superseded_query], 'requests': []}
    service = sampler = None
    tasks = []
    port = None
    learned = oracle
    try:
        port, token = free_port(), secrets.token_hex(24)
        command = [str(binary), 'serve', '--addr', f'127.0.0.1:{port}', '--backend', 'memory']
        raw['command'] = command
        started = time.monotonic()
        raw['started_monotonic'] = started
        service = OwnedService(command, {**os.environ, 'MACEVERY_DB': str(database),
            'MACEVERY_AUTH_TOKEN': token, 'MACEVERY_EXIT_WITH_PARENT': '1',
            'MACEVERY_MAX_ACTIVE_SEARCHES': '2', 'MACEVERY_MAX_ACTIVE_REQUESTS': '64'}, directory)
        raw['owned_process'] = {'pid': service.process.pid, 'parent_pid': os.getpid(),
                                'session_and_group_id': service.process.pid, 'role': 'serve'}
        sampler = Sampler(service.process.pid, args.sample_interval, metrics)

        def new_task(label, target):
            task = HttpTask(label, port, target, token, args.timeout, metrics,
                            service.process.pid, directory / 'service.stderr.txt')
            tasks.append(task)
            raw['requests'].append(task.raw)
            return task

        def search(label, query):
            task = new_task(label, '/search?' + urlencode({'q': query, 'limit': args.limit}))
            task.raw['query'] = query
            return task

        def complete_search(label, query, initial=None):
            task = initial or search(label, query)
            for attempt in range(len(BUSY_RETRY_DELAYS) + 1):
                response = task.wait()
                if response.get('status') == 200 and 'error' not in response:
                    rows_from(response, args.limit)
                    return response
                require(response.get('status') == 503 and 'error' not in response,
                        'Search failed without an explicit HTTP 503; see raw receipt')
                require('busy' in response.get('body', '').lower(), '503 lacked explicit busy response')
                require(attempt < len(BUSY_RETRY_DELAYS), 'Backend remained busy after bounded recovery')
                time.sleep(BUSY_RETRY_DELAYS[attempt])
                task = search(f'{label}-retry-{attempt + 1}', query)

        early = final = None
        if name != 'reference':
            early = search('early-code', args.query)
            early.wait_sent()
        ready_task = new_task('strict-ready', '/status')
        if name == 'clear':
            time.sleep(args.cancel_after)
            early.cancel()
        elif name == 'rapid':
            time.sleep(args.cancel_after)
            early.cancel()
            middle = search('superseded-middle', args.superseded_query)
            middle.wait_sent()
            time.sleep(args.cancel_after)
            middle.cancel()
            final = search('latest-code', args.query)
            final.wait_sent()
        elif name == 'hold':
            final = early

        ready_response = ready_task.wait()
        require(ready_response.get('status') == 200 and 'error' not in ready_response,
                'No complete strict-ready HTTP response')
        ready = ready_response.get('json', {})
        require(ready.get('ready') is True and ready.get('stale') is False,
                'Service did not report strict ready')
        require(ready.get('backend') == 'memory' and type(ready.get('records')) is int
                and ready['records'] == expected_total, 'Ready backend/count differs from oracle')
        raw['ready_monotonic'] = ready_response['finished_monotonic']
        ready_sample = ready_response['service_at_finish']
        require('cpu_ns' in ready_sample, 'No ready CPU sample')
        result['metrics'].update({
            'ready_wall_ms': (raw['ready_monotonic'] - started) * 1000,
            'ready_cpu_ms': ready_sample['cpu_ns'] / 1e6,
            'ready_rss_bytes': ready_sample['rss_bytes'],
            'ready_footprint_bytes': ready_sample['physical_footprint_bytes'],
        })
        final_response = complete_search('post-ready-code' if final is None else 'latest-code', args.query, final)
        require(len(rows_from(final_response, args.limit)) == args.expect_count,
                'Code result count differs from --expect-count')
        for task in tasks:
            task.wait()
        scenario_end = time.monotonic()
        scenario_sample = sampler.capture()
        require('cpu_ns' in scenario_sample, 'No scenario CPU sample')
        result['metrics'].update({
            'scenario_wall_ms': (scenario_end - started) * 1000,
            'scenario_cpu_ms': scenario_sample['cpu_ns'] / 1e6,
            'scenario_rss_bytes': scenario_sample['rss_bytes'],
            'scenario_footprint_bytes': scenario_sample['physical_footprint_bytes'],
        })
        raw['scenario_end_monotonic'] = scenario_end

        # The reference's additional requests prepare the warmed oracle AFTER
        # the scenario boundary. Their actual CPU still appears in wait4 totals.
        if name == 'reference':
            learned = {}
            raw['oracle_started_monotonic'] = time.monotonic()
            for ordinal, query in enumerate(dict.fromkeys([args.query, args.superseded_query])):
                response = complete_search(f'warmed-oracle-{ordinal}', query)
                learned[query] = rows_from(response, args.limit)
            raw['oracle_finished_monotonic'] = time.monotonic()
        require(learned is not None, 'Warmed oracle missing')
        for task in tasks:
            response = task.wait()
            if 'query' not in response:
                continue
            if response.get('status') == 200 and 'error' not in response:
                require(rows_from(response, args.limit) == learned[response['query']],
                        'Complete ordered six-field HTTP results differ from warmed oracle')
            elif 'cancel_requested_monotonic' in response:
                # An intentionally closed client has no result to publish or
                # compare. It may already have caused work in the server.
                continue
            else:
                require(response.get('status') == 503 and 'error' not in response
                        and 'busy' in response.get('body', '').lower(),
                        'Unexpected non-cancelled HTTP outcome')
        if early is not None:
            wait_ms = (early.raw.get('headers_monotonic', ready_response['finished_monotonic'])
                       - early.raw['sent_monotonic']) * 1000
            result['loading_overlap_observed'] = early.raw.get('sent_before_loaded_log') is True and wait_ms >= args.min_loading_overlap_ms
            result['loading_overlap_until_receipt_ms'] = wait_ms
            require(args.allow_unobserved_loading or result['loading_overlap_observed'],
                    'Loading overlap not observed; tiny plumbing smoke may opt out explicitly')
        # Observe a bounded tail without generating more HTTP traffic. This
        # captures some work from closed clients, but is not a proof that every
        # disconnected request has drained before the owned service is stopped.
        time.sleep(args.settle_seconds)
        raw['bounded_tail_sample'] = sampler.capture()
        result.update(status='PASS', records=expected_total, result_count=len(final_response['json']),
                      ordered_json_sha256=json_digest(final_response['json']))
    except Exception as error:
        result['error_type'] = type(error).__name__
        raw['failure'] = repr(error)
    finally:
        for task in tasks:
            try:
                task.close()
            except Exception as error:
                result.update(status='FAIL', error_type=type(error).__name__)
                raw.setdefault('cleanup_failures', []).append(repr(error))
        try:
            if sampler is not None:
                sampler.close()
                raw['service_samples'] = sampler.samples
            if service is not None:
                raw['cleanup'] = close_owned(service, port)
                clean = raw['cleanup']
                result['cleanup'] = {key: clean[key] for key in
                    ['pid', 'method', 'returncode', 'process_group_gone', 'port_closed']}
                require(clean['process_group_gone'] and clean['port_closed'], 'Owned group/port remained')
                require(clean['returncode'] == -15, 'Service exited before owned normal shutdown')
                result['metrics']['lifetime_cpu_ms'] = 1000 * (clean['user_cpu_seconds'] + clean['system_cpu_seconds'])
                result['metrics']['lifetime_peak_rss_bytes'] = clean['lifetime_peak_rss_bytes']
                valid = [s for s in raw['service_samples'] if 'rss_bytes' in s]
                result['metrics']['sampled_peak_rss_bytes'] = max((s['rss_bytes'] for s in valid), default=None)
                result['metrics']['sampled_peak_footprint_bytes'] = max((s['physical_footprint_bytes'] for s in valid), default=None)
        except Exception as error:
            result.update(status='FAIL', error_type=type(error).__name__)
            raw['cleanup_failure'] = repr(error)
        result['http_workers_gone'] = all(not task.thread.is_alive() for task in tasks)
        if not result['http_workers_gone']:
            result.update(status='FAIL', error_type='HttpWorkerCleanupError')
        result['requests'] = [request_public(task.raw) for task in tasks]
        result['raw_reference'] = f'raw/{name}/raw.local.json'
        write_json(directory / 'raw.local.json', raw)
    return result, learned


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    for option in ['binary', 'snapshot', 'output']:
        parser.add_argument('--' + option, required=True, type=Path)
    parser.add_argument('--expected-binary-sha256', help='Exact on-disk SHA256, not normalized/unsigned payload SHA')
    parser.add_argument('--query', default='code')
    parser.add_argument('--superseded-query', default='config')
    parser.add_argument('--limit', type=int, default=300)
    parser.add_argument('--expect-count', type=int, default=300)
    parser.add_argument('--timeout', type=float, default=180)
    parser.add_argument('--sample-interval', type=float, default=.01)
    parser.add_argument('--cancel-after', type=float, default=.05, help='Seconds after sent headers before closing old connection')
    parser.add_argument('--min-loading-overlap-ms', type=float, default=100)
    parser.add_argument('--settle-seconds', type=float, default=.5, help='Bounded observation tail without more requests')
    parser.add_argument('--allow-unobserved-loading', action='store_true', help='Tiny-fixture plumbing only; not loading-path acceptance')
    args = parser.parse_args()
    if (sys.platform != 'darwin' or args.limit < 1 or not 0 <= args.expect_count <= args.limit
            or not all(math.isfinite(value) for value in [args.timeout, args.sample_interval,
                       args.cancel_after, args.min_loading_overlap_ms, args.settle_seconds])
            or args.timeout <= 0 or not .001 <= args.sample_interval <= 1
            or not 0 <= args.cancel_after <= 1 or args.min_loading_overlap_ms < 0
            or not 0 <= args.settle_seconds <= 5
            or not args.query.strip() or not args.superseded_query.strip()
            or contains_time_filter(args.query) or contains_time_filter(args.superseded_query)):
        parser.error('Requires macOS, valid limits and non-relative-time queries')
    source = require_tmp(args.snapshot.resolve(strict=True))
    require(not nonempty_wal(source), 'Use a frozen snapshot without nonempty WAL')
    journal = Path(str(source) + '-journal')
    require(not journal.exists() or journal.stat().st_size == 0, 'Snapshot has active rollback journal')
    binary = args.binary.resolve(strict=True)
    require(binary.is_file() and os.access(binary, os.X_OK), 'Binary is not executable')
    binary_before = identity(binary, True)
    if args.expected_binary_sha256:
        require(binary_before['sha256'] == args.expected_binary_sha256, 'Frozen binary SHA256 differs')
    output = require_tmp(args.output)
    require(not output.exists(), 'Output must be fresh; previous failures are retained')
    output.mkdir(mode=0o700, parents=True)
    (output / 'raw').mkdir(mode=0o700)
    source_before, source_sha = db_identity(source), sha256(source)
    manifest = {'arguments': {key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items()},
                'binary': binary_before, 'source_before': source_before}
    report = {'status': 'FAIL', 'scope': SCOPE, 'gui_publish_validation': 'NOT_RUN',
              'binary_sha256': binary_before['sha256'], 'snapshot_sha256': source_sha,
              'query_sha256': digest_text(args.query), 'superseded_query_sha256': digest_text(args.superseded_query),
              'limit': args.limit, 'expect_count': args.expect_count,
              'allow_unobserved_loading': args.allow_unobserved_loading, 'cases': [],
              'helper_sha256': {name: sha256(Path(__file__).with_name(name)) for name in
                  ['check_startup_early.py', 'check_startup_round6.py', 'check_service_round1.py', 'check_service_round2.py']},
              'notes': [
                  'Backend only: no GUI result publication, debounce, CLI fallback or system sleep is tested.',
                  'A closed connection may still cause server work; cancellation does not imply zero server CPU.',
                  'A bounded no-request tail is observed; complete draining of disconnected server work is not asserted.',
                  'No HTTP response is distinct from an actual observed 503; the client waits beyond the GUI 15s threshold.',
                  'Early send precedes the observed load-log receipt; timing includes controller/scheduling effects.',
                  'Scenario CPU excludes extra reference oracle requests; wait4 lifetime CPU includes every actual request.',
                  'Reference lifetime includes additional oracle preparation and is not an equal-workload performance baseline.',
                  'CPU markers are process cumulative, not isolated per-request CPU under concurrency.',
                  'RSS/footprint sampling can miss peaks; process lifetime HWM comes from wait4.',
                  'Only summary.json is publishable; manifest/raw retain private paths and complete results.',
              ]}
    scratch_name = None
    try:
        metrics = ProcessMetrics()
        manifest['mach_timebase'] = {'numer': metrics.timebase.numer, 'denom': metrics.timebase.denom}
        with tempfile.TemporaryDirectory(prefix='macevery-early-http-', dir='/private/tmp') as scratch_name:
            scratch = Path(scratch_name)
            prepared = scratch / 'prepared' / 'index.sqlite'
            manifest['preparation_copy'] = clone_or_backup(source, prepared, args.timeout)
            prep_dir = output / 'raw' / 'preparation-status'
            prep_dir.mkdir()
            prep = {}
            try:
                status_cli(binary, prepared, prep_dir, args, metrics, prep)
                require(prep['total'] > 0, 'Nonempty snapshot required')
            finally:
                write_json(prep_dir / 'raw.local.json', prep)
            with closing(sqlite3.connect(prepared)) as connection:
                connection.execute('PRAGMA wal_checkpoint(TRUNCATE)')
            require(not nonempty_wal(prepared), 'Prepared copy has nonempty WAL')
            prepared_sha = sha256(prepared)
            oracle = None
            for name in ['reference', 'hold', 'clear', 'rapid']:
                database = scratch / name / 'index.sqlite'
                copied = clone_or_backup(prepared, database, args.timeout)
                directory = output / 'raw' / name
                directory.mkdir()
                write_json(directory / 'copy.local.json', copied)
                result, oracle = run_case(name, binary, database, directory, args, metrics, prep['total'], oracle)
                report['cases'].append(result)
                if name == 'reference':
                    require(result['status'] == 'PASS' and oracle is not None, 'Reference/oracle failed; see retained receipt')
                    report['warmed_oracles'] = {digest_text(query): {'count': len(rows), 'ordered_json_sha256': json_digest(rows)}
                                                for query, rows in oracle.items()}
            require(sha256(prepared) == prepared_sha, 'Shared prepared input changed')
        require(len(report['cases']) == 4 and all(case['status'] == 'PASS' for case in report['cases']),
                'One or more backend cases failed; all receipts retained')
        report.update(status='PASS', ordered_results_match=True,
                      loading_path_acceptance=('PASS' if all(case.get('loading_overlap_observed') is True
                                               for case in report['cases'] if case['case'] != 'reference')
                                               else 'NOT_EXERCISED'))
    except Exception as error:
        report['error_type'] = type(error).__name__
        (output / 'driver-failure.local.txt').write_text(repr(error) + '\n')
    finally:
        if scratch_name is not None:
            report['temporary_copies_removed'] = not Path(scratch_name).exists()
        try:
            report['source_unchanged'] = source_before == db_identity(source) and source_sha == sha256(source)
            report['binary_unchanged'] = binary_before == identity(binary, True)
            if not report['source_unchanged'] or not report['binary_unchanged']:
                report.update(status='FAIL', integrity_error_type='InputChanged')
        except Exception as error:
            report.update(status='FAIL', integrity_error_type=type(error).__name__)
        write_json(output / 'manifest.local.json', manifest)
        write_json(output / 'summary.json', report)
    print(json.dumps({'status': report['status'], 'cases': len(report['cases']),
                      'gui_publish_validation': 'NOT_RUN'}))
    return 0 if report['status'] == 'PASS' else 1


if __name__ == '__main__':
    raise SystemExit(main())
