#!/usr/bin/env python3
"""Isolated macOS status -> memory-service startup A/B/A (no GUI or watcher).

Example (root schedules all performance work sequentially):
  python3 tools/check_startup_round6.py --baseline /tmp/base/macevery \
    --candidate /tmp/new/macevery --snapshot /tmp/frozen/index.sqlite \
    --rounds 2 --order aba --query code --limit 300 \
    --output /tmp/macevery-startup-round6

This approximates the current Swift bootstrap gate, not window appearance or a
rendered first frame. A candidate-migrated temporary copy is prepared outside
timing; each process sequence gets a fresh clone of that same prepared input.
Only frozen /tmp input and fresh /tmp output are accepted. No installed App,
watcher, real filesystem roots, preferences, hotkey or fixed live port is used.

summary.json contains hashes/counts/metrics and relative raw references only.
manifest.local.json and raw/ contain private paths, status output, and complete
ordered HTTP results: keep the output directory local and publish only summary.
The HTTP digest covers every returned field (including score) and array order,
not the additional FileRecord fields absent from the HTTP API. Relative-time
queries are rejected because this API has no fixed-now parameter.

CPU excludes the controller/copy/preparation. Status CPU and lifetime HWM use
wait4; service milestone CPU is cumulative libproc CPU since process creation.
RSS/footprint samples may miss short peaks; wait4 HWM is process-lifetime RSS.
Milestone wall times include controller scheduling/polling and HTTP encoding.
Caches remain uncontrolled; all samples and surrounding-A comparisons remain.
"""
import argparse
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import signal
import sqlite3
import sys
import tempfile
import time
from urllib.parse import urlencode

from check_service_round1 import json_digest
from check_service_round2 import (
    OwnedService, ProcessMetrics, Sampler, clone_or_backup, db_identity, free_port,
    identity, nonempty_wal, request, require, require_tmp, response_json, sha256,
    write_json,
)
from compare_round1 import comparison, describe


SCOPE = 'isolated status CLI -> memory service approximation; no GUI/window/watcher'
HTTP_FIELDS = {'path', 'basename', 'kind', 'size', 'mtime', 'score'}


def contains_time_filter(query):
    # The Rust tokenizer removes quotes/escapes before recognizing filters.
    # Conservatively reject any such spelling: this HTTP API has no fixed now.
    normalized = query.translate(str.maketrans('', '', "'\"\\"))
    return re.search(r'(?:mtime|modified):', normalized, re.I) is not None


def close_owned(child, port):
    receipt = child.close(port)
    if not receipt['process_group_gone']:
        # The primary may have exited while a descendant retained its session.
        # Kill only the process group created by this exact OwnedService.
        try:
            os.killpg(child.process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            try:
                os.killpg(child.process.pid, 0)
            except ProcessLookupError:
                break
            time.sleep(.02)
        receipt = {**child.close(port), 'descendant_SIGKILL': True}
    return receipt


def status_cli(binary, database, directory, args, metrics, raw):
    """Do not poll()/wait(): OwnedService.reap owns wait4 and its exact rusage."""
    child = sampler = None
    started = time.monotonic()
    raw['command'] = [str(binary), 'status', '--json']
    try:
        child = OwnedService(raw['command'], {**os.environ, 'MACEVERY_DB': str(database)}, directory)
        sampler = Sampler(child.process.pid, args.sample_interval, metrics)
        while child.reap() is None:
            require(time.monotonic() - started < args.timeout, 'Status CLI timed out')
            time.sleep(args.sample_interval)
        raw['wall_ms'] = (time.monotonic() - started) * 1000
    finally:
        if sampler is not None:
            sampler.close()
            raw['samples'] = sampler.samples
        if child is not None:
            raw['cleanup'] = close_owned(child, 0)  # This CLI has no listening port.
    require(raw['cleanup']['process_group_gone'], 'Status CLI process group remained')
    require(raw['cleanup']['returncode'] == 0, 'Status CLI failed')
    stdout = directory / 'service.stdout.txt'
    require(stdout.stat().st_size <= 8 * 1024 * 1024, 'Status output too large')
    value = json.loads(stdout.read_text())
    counts = {key: value[key] for key in ['files', 'dirs', 'apps', 'symlinks', 'others']}
    require(all(isinstance(v, int) and v >= 0 for v in counts.values()), 'Invalid status counts')
    raw['counts'], raw['total'] = counts, sum(counts.values())
    raw['cpu_ms'] = 1000 * sum(raw['cleanup'][key] for key in ['user_cpu_seconds', 'system_cpu_seconds'])
    return value


def public_status_metrics(raw):
    samples = [s for s in raw['samples'] if 'rss_bytes' in s]
    latest = samples[-1] if samples else {}
    return {
        'status_wall_ms': raw['wall_ms'], 'status_cpu_ms': raw['cpu_ms'],
        'status_lifetime_peak_rss_bytes': raw['cleanup']['lifetime_peak_rss_bytes'],
        'status_last_sample_rss_bytes': latest.get('rss_bytes'),
        'status_last_sample_footprint_bytes': latest.get('physical_footprint_bytes'),
    }


def run_one(binary, variant, block, ordinal, database, directory, args, metrics):
    run_id = f'run-{ordinal:02d}-{variant}'
    result = {'run_id': run_id, 'variant': variant, 'block': block, 'status': 'FAIL', 'metrics': {}}
    raw = {'scope': SCOPE, 'status_cli': {}, 'query': args.query, 'database': str(database)}
    service = sampler = None
    for part in ['status', 'serve']:
        (directory / part).mkdir(parents=True)
    bootstrap_started = time.monotonic()
    try:
        status_cli(binary, database, directory / 'status', args, metrics, raw['status_cli'])
        status = raw['status_cli']
        require(status['total'] > 0, 'Nonempty snapshot required for this Swift bootstrap approximation')
        result.update(records=status['total'], counts=status['counts'])
        result['metrics'].update(public_status_metrics(status))
        port, token = free_port(), secrets.token_hex(24)
        raw['serve_command'] = [str(binary), 'serve', '--addr', f'127.0.0.1:{port}', '--backend', 'memory']
        environment = {**os.environ, 'MACEVERY_DB': str(database), 'MACEVERY_AUTH_TOKEN': token,
                       'MACEVERY_EXIT_WITH_PARENT': '1'}
        serve_started = time.monotonic()
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
                    require(ready['records'] == status['total'], 'Loaded count differs from status CLI')
                    break
            require(response.get('status') != 401, 'Service token rejected')
            time.sleep(min(args.sample_interval, max(0, deadline - time.monotonic())))
        ready_at = time.monotonic()
        ready_sample = sampler.capture()
        require('cpu_ns' in ready_sample, 'Cannot sample ready service')
        result['metrics'].update({
            'serve_ready_wall_ms': (ready_at - serve_started) * 1000,
            'serve_ready_cpu_ms': ready_sample['cpu_ns'] / 1e6,
            'serve_ready_rss_bytes': ready_sample['rss_bytes'],
            'serve_ready_footprint_bytes': ready_sample['physical_footprint_bytes'],
            'bootstrap_ready_wall_ms': (ready_at - bootstrap_started) * 1000,
            'bootstrap_ready_cpu_ms': status['cpu_ms'] + ready_sample['cpu_ns'] / 1e6,
        })
        query_started = time.monotonic()
        raw['first_search'] = request(port, '/search?' + urlencode({'q': args.query, 'limit': args.limit}), token, args.timeout)
        query_finished = time.monotonic()
        query_sample = sampler.capture()
        require('cpu_ns' in query_sample, 'Cannot sample first-search service')
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
            'bootstrap_first_search_wall_ms': (query_finished - bootstrap_started) * 1000,
            'bootstrap_first_search_cpu_ms': status['cpu_ms'] + query_sample['cpu_ns'] / 1e6,
        })
        result['status'] = 'PASS'
    except Exception as error:
        result['error_type'] = type(error).__name__
        raw['failure'] = repr(error)
    finally:
        status_clean = raw['status_cli'].get('cleanup')
        if status_clean is not None:
            result['status_cleanup'] = {key: status_clean[key] for key in
                                        ['pid', 'method', 'returncode', 'process_group_gone']}
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
                if 'cpu_ms' in raw['status_cli']:
                    result['metrics']['bootstrap_lifetime_cpu_ms'] = raw['status_cli']['cpu_ms'] + result['metrics']['serve_lifetime_cpu_ms']
        except Exception as error:
            result.update(status='FAIL', error_type=type(error).__name__)
            raw['cleanup_failure'] = repr(error)
        write_json(directory / 'raw.local.json', raw)
    result['raw_reference'] = f'raw/{run_id}/raw.local.json'
    return result


def summarize(runs, order):
    passed = [run for run in runs if run['status'] == 'PASS']
    keys = sorted({key for run in passed for key, value in run['metrics'].items() if value is not None})
    comparisons, paired, blocks = {}, {}, {}
    for key in keys:
        values = {variant: [r['metrics'][key] for r in passed if r['variant'] == variant and r['metrics'].get(key) is not None]
                  for variant in ['baseline', 'candidate']}
        if all(values.values()):
            comparisons[key] = comparison(values['baseline'], values['candidate'])
        changes = []
        for block in sorted({r['block'] for r in passed}):
            subset = [r for r in passed if r['block'] == block]
            a = [r['metrics'][key] for r in subset if r['variant'] == 'baseline' and r['metrics'].get(key) is not None]
            b = [r['metrics'][key] for r in subset if r['variant'] == 'candidate' and r['metrics'].get(key) is not None]
            expected_a = 2 if order == 'aba' else 1
            if len(a) == expected_a and len(b) == 1:
                entry = comparison(a, b)
                blocks.setdefault(block, {})[key] = {
                    'baseline_median': entry['baseline']['median'],
                    'candidate_median': entry['candidate']['median'],
                    'candidate_change_pct': entry['candidate_change_pct'],
                }
                delta = entry['candidate_change_pct']
                if delta is not None:
                    changes.append(delta)
        if changes:
            paired[key] = describe(changes)
    return {'metrics': comparisons, 'paired_block_candidate_change_pct': paired,
            'paired_blocks': [{'block': block,
                               'baseline_runs': [r['run_id'] for r in passed if r['block'] == block and r['variant'] == 'baseline'],
                               'candidate_runs': [r['run_id'] for r in passed if r['block'] == block and r['variant'] == 'candidate'],
                               'metrics': values} for block, values in sorted(blocks.items())]}


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    for option in ['baseline', 'candidate', 'snapshot', 'output']:
        parser.add_argument('--' + option, type=Path, required=True)
    parser.add_argument('--rounds', type=int, default=2, help='Number of independent AB/ABA blocks')
    parser.add_argument('--order', choices=['ab', 'aba'], default='aba')
    parser.add_argument('--query', default='code')
    parser.add_argument('--limit', type=int, default=300)
    parser.add_argument('--expect-count', type=int, help='Optional exact first-query count assertion')
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
               ['check_startup_round6.py', 'check_service_round1.py', 'check_service_round2.py', 'compare_round1.py']}
    runs, report = [], {'status': 'FAIL', 'scope': SCOPE, 'helper_sha256': helpers,
                       'snapshot_sha256': manifest['snapshot_sha256'],
                       'binary_sha256': {key: value['sha256'] for key, value in manifest['binaries'].items()},
                       'query_sha256': hashlib.sha256(args.query.encode()).hexdigest(), 'limit': args.limit,
                       'rounds': args.rounds, 'order': args.order, 'runs': [],
                       'notes': ['status -> serve approximates the current Swift gate, not a window/first-frame measurement.',
                                 'No watcher, preferences, fixed live port or real filesystem roots are used.',
                                 'Negative candidate change means lower time/CPU/bytes; all samples are retained.',
                                 'HTTP hashes cover all returned fields/order, not hidden FileRecord fields.',
                                 'Service milestone CPU is cumulative since spawn; lifecycle CPU/HWM includes shutdown.',
                                 'Last status RSS/footprint is the last valid sample, not an exact-at-exit reading.',
                                 'Sampled RSS may miss peaks; capacity, RSS and footprint are different metrics.',
                                 'Copy/migration/controller CPU is excluded; caches and scheduling remain uncontrolled.',
                                 'Only summary.json is suitable for publication; manifest/raw contain private paths/results.']}
    scratch_name = None
    try:
        metrics = ProcessMetrics()
        manifest['mach_timebase'] = {'numer': metrics.timebase.numer, 'denom': metrics.timebase.denom}
        with tempfile.TemporaryDirectory(prefix='macevery-startup-round6-', dir='/private/tmp') as scratch_name:
            scratch = Path(scratch_name)
            prepared = scratch / 'prepared' / 'index.sqlite'
            manifest['preparation_copy'] = clone_or_backup(source, prepared, args.timeout)
            prep_dir = output / 'raw' / 'preparation-status'
            prep_dir.mkdir()
            prep = {}
            try:
                status_cli(binaries['candidate'], prepared, prep_dir, args, metrics, prep)
            finally:
                write_json(prep_dir / 'raw.local.json', prep)
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
                    run = run_one(binaries[variant], variant, block, ordinal, database, directory, args, metrics)
                    runs.append(run)
                    require(run['status'] == 'PASS', f'{run_id} failed; see local raw receipt')
            require(sha256(prepared) == manifest['prepared_sha256'], 'Prepared shared input changed')
        report['temporary_copies_removed'] = True
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
