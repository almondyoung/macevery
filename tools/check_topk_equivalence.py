#!/usr/bin/env python3
"""Compare frozen/candidate CLI and HTTP memory/sqlite Top300 on /tmp copies.

  python3 tools/check_topk_equivalence.py --baseline /tmp/frozen/macevery \
    --candidate /tmp/candidate/macevery --snapshot /tmp/frozen/index.sqlite \
    --output /tmp/macevery-exact-topk

Only root schedules real-data execution. This tool never opens the source DB
writable, changes an App, starts a watcher, or runs two resident indexes at once.
Candidate migration and deterministic SQL probe preparation occur outside timed
queries, only on a /tmp clone. Every version/path gets a fresh identical copy.
Optional --prepare-compact explicitly runs candidate optimize once, after schema
migration and before probe insertion. Both versions then copy that same new DB.
Its CLI CPU/HWM/wall and byte counts are separate preparation costs. Source and
output preparation DBs remain in private raw evidence, including on failure.
This physical layout change is not a code-only speedup or a cache-cold run.
Initial queries cover code300, filters, glob, filtered fuzzy, Unicode and nohit,
plus real extension/glob/fuzzy/OR/exclusion/path/part/multiple-term queries;
three transactions rename, modify and delete probes, with code/probe rechecks.
All queries are time-independent; relative mtime/modified filters are rejected.

The frozen baseline memory HTTP results are the full-index oracle, per stage.
Candidate CLI, memory and sqlite must match all six fields and array order.
Legacy baseline CLI/sqlite differences are retained as existing bounded-pool
behavior, not excused for the candidate. Baseline memory and candidate memory
must also agree. CPU cost for correcting legacy recall is not equal-workload
speedup evidence; this is one fixed sequential A/B mechanism run, not A/B/A.

CLI CPU/HWM use wait4, including process/init/encoding. HTTP query CPU/RSS uses
libproc/Mach timebase, including refresh and encoding. On an HTTP timeout the
owned service is closed immediately; later queries are NOT_RUN, so abandoned
work cannot overlap the next measured query. Lifetime CPU includes consumed
work until shutdown, not an assertion that timed-out work completed. The actual
query timeout defaults to 15s; a longer override still reports the 15s GUI budget.
Overall PASS requires both exact results and all candidate queries within 15s.

summary.json contains numeric metrics/counts/hashes/relative raw references.
manifest.local.json and raw/ retain private paths, full results and failures.
No failed query is removed from evidence or silently treated as equivalent.
"""
import argparse
from contextlib import closing
import hashlib
import json
import math
import os
from pathlib import Path
import secrets
import sqlite3
import sys
import tempfile
import time
from urllib.parse import urlencode

from check_service_round1 import json_digest
from check_service_round2 import (
    OwnedService, ProcessMetrics, Sampler, clone_or_backup, db_identity, free_port,
    identity, nonempty_wal, readonly, request, require, require_tmp, sha256, write_json,
)
from check_startup_round6 import HTTP_FIELDS, close_owned, contains_time_filter


FROZEN_BASELINE_SHA = '422e776c583ff9769c613c5f915f1b5c4e993da4085fa434042b81bd82a710ee'
ROOT = '/__MacEvery_TopK_Equivalence__'
EXTENSION = 'topkeq'
STAGES = ['initial', 'rename', 'modify', 'delete']
INITIAL_QUERIES = [
    ('code', 'code'),
    ('filters', 'ext:topkeq kind:file !path:skip tpkq'),
    ('glob', '*.topkeq'),
    ('fuzzy', 'ext:topkeq ~tpq'),
    ('unicode', 'ext:topkeq 特殊'),
    ('nohit', '__macevery_topk_equivalence_nohit_7d28f4__'),
    ('probes', 'ext:topkeq'),
    ('real_filtered_code', 'ext:rs code'),
    ('real_glob', '*.rs'),
    ('real_fuzzy', 'ext:rs ~code'),
    ('real_ext_or', 'ext:rs|swift code'),
    ('real_ext_include_exclude', 'ext:rs|swift !ext:swift code'),
    ('real_path', 'path:src code'),
    ('real_part', 'part:src code'),
    ('real_terms', 'code test'),
]
UPDATED_QUERIES = [INITIAL_QUERIES[i] for i in [0, 4, 6]]


def rows(value, limit):
    require(isinstance(value, list) and len(value) <= limit, 'Invalid result array')
    require(all(isinstance(row, dict) and set(row) == HTTP_FIELDS for row in value),
            'Expected exactly six public result fields')
    for row in value:
        require(all(isinstance(row[key], str) for key in ['path', 'basename', 'kind']), 'Invalid string fields')
        require(all(row[key] is None or type(row[key]) is int for key in ['size', 'mtime']), 'Invalid nullable integers')
        require(type(row['score']) is int, 'Invalid score')
    return value


def compact_outcome(outcome):
    return {key: value for key, value in outcome.items() if key not in ('json', 'response', 'failure')}


def run_cli(binary, database, arguments, directory, timeout, interval, metrics):
    directory.mkdir(parents=True)
    raw = {'command': [str(binary), *arguments]}
    outcome = {'status': 'FAIL', 'timed_out': False, 'child_started': False, 'cleanup_verified': True, 'metrics': {}}
    child = sampler = None
    started = time.monotonic()
    try:
        child = OwnedService(raw['command'], {**os.environ, 'MACEVERY_DB': str(database)}, directory)
        outcome.update(child_started=True, cleanup_verified=False)
        sampler = Sampler(child.process.pid, interval, metrics)
        while child.reap() is None:
            if time.monotonic() - started >= timeout:
                outcome['timed_out'] = True
                break
            time.sleep(interval)
        outcome['metrics']['wall_seconds'] = time.monotonic() - started
    except Exception as error:
        raw['failure'] = repr(error)
        outcome['error_type'] = type(error).__name__
    finally:
        outcome['metrics'].setdefault('wall_seconds', time.monotonic() - started)
        if sampler is not None:
            try:
                sampler.close()
            except Exception as error:
                raw['sampler_failure'] = repr(error)
                outcome['sampler_error_type'] = type(error).__name__
            raw['samples'] = sampler.samples
        if child is not None:
            try:
                raw['cleanup'] = close_owned(child, 0)
                clean = raw['cleanup']
                outcome['cleanup_verified'] = clean['process_group_gone']
                outcome['cleanup'] = {key: clean[key] for key in ['pid', 'returncode', 'method', 'process_group_gone']}
                valid = [sample for sample in raw.get('samples', []) if 'rss_bytes' in sample]
                outcome['metrics'].update(
                    cpu_seconds=clean['user_cpu_seconds'] + clean['system_cpu_seconds'],
                    process_peak_rss_bytes=clean['lifetime_peak_rss_bytes'],
                    sampled_peak_rss_bytes=max((sample['rss_bytes'] for sample in valid), default=None),
                    end_rss_bytes=valid[-1]['rss_bytes'] if valid else None,
                    end_footprint_bytes=valid[-1]['physical_footprint_bytes'] if valid else None,
                    sampled_peak_footprint_bytes=max((sample['physical_footprint_bytes'] for sample in valid), default=None),
                )
                require(clean['process_group_gone'], 'Owned CLI group remained')
                require(not outcome['timed_out'] and clean['returncode'] == 0, 'CLI timed out or failed')
                require('sampler_failure' not in raw, 'CLI sampler failed')
                require('failure' not in raw, 'CLI control path failed')
                stdout = directory / 'service.stdout.txt'
                require(stdout.stat().st_size <= 8 * 1024 * 1024, 'CLI output exceeded evidence limit')
                outcome['json'] = json.loads(stdout.read_text())
                outcome['status'] = 'PASS'
            except Exception as error:
                raw['failure'] = repr(error)
                outcome['error_type'] = type(error).__name__
        outcome['exceeded_gui_15s'] = outcome['metrics'].get('wall_seconds', 0) > 15
        raw['outcome'] = compact_outcome(outcome)
        write_json(directory / 'raw.local.json', raw)
    return outcome


def seed_probes(database):
    require_tmp(database)
    with closing(sqlite3.connect(database)) as connection:
        require(connection.execute('SELECT count(*) FROM files WHERE ext_lower=?', (EXTENSION,)).fetchone()[0] == 0,
                'Source already contains reserved topkeq rows')
        probes = [
            ('MiXeD/tpkq.topkeq', 'tpkq.topkeq', 'file', 11, 100),
            ('MiXeD/delete-tpkq.topkeq', 'delete-tpkq.topkeq', 'file', 22, 100),
            ('特殊/特殊-tpkq.topkeq', '特殊-tpkq.topkeq', 'file', None, None),
            ('skip/tpkq.topkeq', 'tpkq.topkeq', 'file', 33, 100),
            ('tie-a/tpkq.topkeq', 'tpkq.topkeq', 'file', 44, 100),
            ('tie-b/tpkq.topkeq', 'tpkq.topkeq', 'file', 44, 100),
        ]
        for relative, basename, kind, size, modified in probes:
            path = ROOT + '/' + relative
            connection.execute(
                'INSERT INTO files(path,path_lower,basename,basename_lower,ext_lower,kind,size,mtime,ctime,dev,inode,indexed_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)',
                (path, path.lower(), basename, basename.lower(), EXTENSION, kind, size, modified, 90, 1, 100, 1700000000000000000))
        connection.commit()
        connection.execute('PRAGMA wal_checkpoint(TRUNCATE)')
    return len(probes)


def apply_stage(database, stage):
    # External DML is explicitly confined to this tool's owned /tmp clone.
    require_tmp(database)
    with closing(sqlite3.connect(database)) as connection:
        connection.execute('BEGIN IMMEDIATE')
        if stage == 'rename':
            old, new = ROOT + '/MiXeD/tpkq.topkeq', ROOT + '/MiXeD/tpkq-renamed.topkeq'
            changed = connection.execute('UPDATE files SET path=?,path_lower=?,basename=?,basename_lower=? WHERE path=?',
                (new, new.lower(), 'tpkq-renamed.topkeq', 'tpkq-renamed.topkeq', old)).rowcount
        elif stage == 'modify':
            changed = connection.execute('UPDATE files SET size=?,mtime=? WHERE path=?',
                (4097, 1700001234, ROOT + '/MiXeD/tpkq-renamed.topkeq')).rowcount
        elif stage == 'delete':
            changed = connection.execute('DELETE FROM files WHERE path=?', (ROOT + '/MiXeD/delete-tpkq.topkeq',)).rowcount
        else:
            raise AssertionError('Unknown update stage')
        require(changed == 1, 'Probe transaction did not change exactly one row')
        connection.commit()


def stage_queries(stage):
    return INITIAL_QUERIES if stage == 'initial' else UPDATED_QUERIES


def query_key(stage, label):
    return stage + '/' + label


def preparation_state(database):
    """Small read-only snapshot; no file-table scan or schema initialization."""
    with closing(readonly(database)) as connection:
        connection.execute('BEGIN DEFERRED')
        try:
            state = connection.execute('SELECT * FROM macevery_content_state WHERE singleton=1')
            columns = [column[0] for column in state.description]
            values = state.fetchall()
            require(len(values) == 1, 'Expected one content-state row')
            content = dict(zip(columns, values[0]))
            require(isinstance(content.get('database_id'), str) and type(content.get('revision')) is int,
                    'Invalid preparation content stamp')
            result = {'content_state': content,
                      'roots': connection.execute('SELECT path FROM roots ORDER BY path').fetchall(),
                      'excludes': connection.execute('SELECT pattern FROM excludes ORDER BY pattern').fetchall(),
                      'meta': connection.execute('SELECT key,value FROM meta ORDER BY key').fetchall()}
            connection.commit()
            return result
        except BaseException:
            connection.rollback()
            raise


def public_preparation_state(state):
    # Config and database identity are private. Expose counts/digests and the
    # revision, while the full values remain available for local verification.
    return {'content_state_sha256': json_digest(state['content_state']),
            'revision': state['content_state']['revision'],
            **{key: {'count': len(state[key]), 'ordered_json_sha256': json_digest(state[key])}
               for key in ['roots', 'excludes', 'meta']}}


def prepare_compacted_copy(binary, source, destination, directory, args, metrics, expected_records, receipt):
    require_tmp(source)
    require_tmp(destination)
    require(not destination.exists(), 'Compaction requires a fresh output DB')
    destination.parent.mkdir(parents=True)
    raw = {'source': str(source), 'output': str(destination)}
    receipt.update(status='FAIL', excluded_from_query_timing=True,
                   single_preparation_shared_by_six_views=True,
                   expected_records_before_probe_seed=expected_records,
                   all_file_records_verified_by_cli=False,
                   source_raw_reference='raw/prepared-source/index.sqlite',
                   output_raw_reference='raw/prepared-compact/index.sqlite',
                   raw_reference='raw/compaction/compaction.local.json')
    before_sha = before_identity = before_state = None
    started = time.monotonic()
    try:
        require(not nonempty_wal(source), 'Prepared source must have no nonempty WAL')
        before_state = preparation_state(source)
        before_sha, before_identity = sha256(source), identity(source)
        raw.update(source_before_identity=before_identity, source_before_state=before_state)
        receipt.update(source_sha256_before=before_sha, source_main_bytes=before_identity['bytes'],
                       before=public_preparation_state(before_state))
        outcome = run_cli(binary, source, ['optimize', '--output', str(destination), '--json'], directory,
                          args.startup_timeout, args.sample_interval, metrics)
        raw['cli_outcome'] = outcome
        receipt.update(cli_status=outcome['status'], metrics=outcome['metrics'],
                       cleanup_verified=outcome['cleanup_verified'])
        require(outcome['cleanup_verified'], 'Compaction CLI group remained')
        require(outcome['status'] == 'PASS' and not outcome['timed_out'], 'Explicit compaction CLI failed')
        value = outcome['json']
        require(isinstance(value, dict) and set(value) == {
                    'optimized_copy', 'source_main_bytes', 'output_main_bytes',
                    'wall_ms', 'source_unchanged', 'verified_records'}
                and value.get('optimized_copy') is True
                and value.get('source_unchanged') is True, 'Invalid compaction receipt')
        require(type(value['verified_records']) is int and value['verified_records'] == expected_records,
                'Compaction did not verify every pre-seed record')
        receipt['verified_records'] = value['verified_records']
        require(destination.is_file() and not nonempty_wal(destination), 'No standalone compacted output')
        after_state = preparation_state(destination)
        raw['output_state'] = after_state
        output_identity = identity(destination)
        raw['output_identity'] = output_identity
        require(value.get('source_main_bytes') == before_identity['bytes']
                and value.get('output_main_bytes') == output_identity['bytes'], 'Compaction byte counts differ')
        require(type(value.get('wall_ms')) is int and value['wall_ms'] >= 0, 'Invalid compaction wall time')
        receipt.update(output_main_bytes=output_identity['bytes'], cli_reported_wall_ms=value['wall_ms'],
                       output_sha256_before_probe_seed=sha256(destination), after=public_preparation_state(after_state),
                       content_stamp_and_configuration_equal=before_state == after_state)
        require(before_state == after_state, 'Compaction changed ID/revision or control metadata')
        receipt['status'] = 'PASS'
    except BaseException as error:
        receipt.update(status='FAIL', error_type=type(error).__name__)
        raw['failure'] = repr(error)
        raise
    finally:
        try:
            after_sha = sha256(source)
            after_identity = identity(source)
            after_source_state = preparation_state(source)
            raw.update(source_after_identity=after_identity, source_after_state=after_source_state)
            receipt.update(source_sha256_after=after_sha,
                           source_copy_unchanged=before_sha == after_sha and before_identity == after_identity
                           and before_state == after_source_state and not nonempty_wal(source))
            if not receipt['source_copy_unchanged']:
                receipt.update(status='FAIL', integrity_error_type='PreparedSourceChanged')
        except BaseException as error:
            receipt.update(status='FAIL', integrity_error_type=type(error).__name__)
            raw['integrity_failure'] = repr(error)
        receipt.update(end_to_end_wall_seconds=time.monotonic() - started,
                       source_copy_retained=source.exists(), output_copy_retained=destination.exists())
        directory.mkdir(parents=True, exist_ok=True)
        write_json(directory / 'compaction.local.json', raw)
    require(receipt['status'] == 'PASS', 'Compaction source-copy integrity check failed')
    receipt['all_file_records_verified_by_cli'] = True
    return destination


def validate_probe_rows(database, data):
    with closing(readonly(database)) as connection:
        expected = {row[0]: row for row in connection.execute(
            'SELECT path,basename,kind,size,mtime FROM files WHERE ext_lower=?', (EXTENSION,))}
    actual = {row['path']: tuple(row[key] for key in ['path', 'basename', 'kind', 'size', 'mtime']) for row in data}
    require(len(actual) == len(data) and actual == expected, 'Probe results do not reflect committed transaction')


def run_view(binary, variant, mode, database, directory, args, metrics, total):
    directory.mkdir()
    raw = {'variant': variant, 'mode': mode, 'database': str(database), 'phases': {}}
    result = {'variant': variant, 'mode': mode, 'status': 'FAIL', 'phases': {}, 'stage_status': {}, 'cleanup_verified': True}
    service = sampler = None
    port = None
    try:
        if mode != 'cli':
            port, token = free_port(), secrets.token_hex(24)
            command = [str(binary), 'serve', '--addr', f'127.0.0.1:{port}', '--backend', mode]
            raw['command'] = command
            started = time.monotonic()
            service = OwnedService(command, {**os.environ, 'MACEVERY_DB': str(database),
                'MACEVERY_AUTH_TOKEN': token, 'MACEVERY_EXIT_WITH_PARENT': '1'}, directory)
            result['cleanup_verified'] = False
            sampler = Sampler(service.process.pid, args.sample_interval, metrics)
            raw['readiness_attempts'] = []
            deadline = started + args.startup_timeout
            while True:
                require(service.reap() is None, 'Service exited before readiness')
                remaining = deadline - time.monotonic()
                require(remaining > 0, 'Strict startup timed out')
                response = request(port, '/status', token, remaining)
                raw['readiness_attempts'].append(response)
                status = response.get('json', {})
                if response.get('status') == 200 and isinstance(status, dict) and status.get('ready') is True and status.get('stale') is False:
                    break
                time.sleep(min(.1, max(0, deadline - time.monotonic())))
            require(status.get('ready') is True and status.get('stale') is False
                    and status.get('backend') == mode and status.get('records') == total, 'Strict ready failed')
            require(time.monotonic() <= deadline, 'Startup response exceeded its deadline')
            sample = sampler.capture()
            require('cpu_ns' in sample, 'No service load CPU sample')
            result['load'] = {'wall_seconds': time.monotonic() - started,
                'cpu_seconds': sample['cpu_ns'] / 1e9, 'end_rss_bytes': sample['rss_bytes'],
                'end_footprint_bytes': sample['physical_footprint_bytes']}
        for stage_index, stage in enumerate(STAGES):
            if stage != 'initial':
                apply_stage(database, stage)
            for label, query in stage_queries(stage):
                key = query_key(stage, label)
                if mode == 'cli':
                    outcome = run_cli(binary, database, ['search', query, '--limit', str(args.limit), '--json'],
                                      directory / stage / label, args.query_timeout, args.sample_interval, metrics)
                else:
                    response, measured = sampler.measure(key, lambda: request(port,
                        '/search?' + urlencode({'q': query, 'limit': args.limit}), token, args.query_timeout))
                    samples = [sample for sample in sampler.samples if sample['phase'] == key and 'rss_bytes' in sample]
                    measured.update(start_footprint_bytes=samples[0]['physical_footprint_bytes'] if samples else None,
                                    end_footprint_bytes=samples[-1]['physical_footprint_bytes'] if samples else None,
                                    sampled_peak_footprint_bytes=max((sample['physical_footprint_bytes'] for sample in samples), default=None))
                    outcome = {'status': 'FAIL', 'response': response, 'metrics': measured,
                               'timed_out': 'timed out' in response.get('error', '').lower(),
                               'exceeded_gui_15s': measured['wall_seconds'] > 15}
                    if response.get('status') == 200 and 'error' not in response and 'json' in response:
                        outcome.update(status='PASS', json=response['json'])
                raw['phases'][key] = outcome
                try:
                    require(outcome['status'] == 'PASS', 'Query failed; later phases are not run on this service')
                    data = rows(outcome['json'], args.limit)
                    if label == 'code':
                        require(len(data) == args.expect_code_count, 'Unexpected code result count')
                    if label in ('probes', 'fuzzy'):
                        validate_probe_rows(database, data)
                    if label == 'fuzzy':
                        require(all(row['score'] >= 1000 for row in data), 'Fuzzy fixture did not exercise fuzzy scoring')
                    if label == 'nohit':
                        require(data == [], 'No-hit sentinel unexpectedly matched')
                    if label == 'unicode':
                        require(len(data) == 1 and data[0]['path'] == ROOT + '/特殊/特殊-tpkq.topkeq', 'Unicode probe is missing')
                    if mode != 'cli':
                        require(outcome['metrics']['cpu_seconds'] is not None and outcome['metrics']['cpu_seconds'] >= 0,
                                'HTTP query CPU sample unavailable')
                    outcome.update(count=len(data), ordered_json_sha256=json_digest(data))
                except Exception as error:
                    outcome.update(status='FAIL', error_type=type(error).__name__)
                    outcome['failure'] = repr(error)
                    raise
                finally:
                    result['phases'][key] = compact_outcome(outcome)
            if mode != 'cli':
                after = request(port, '/status', token, args.query_timeout)
                raw.setdefault('stage_status', {})[stage] = after
                require(after.get('status') == 200 and isinstance(after.get('json'), dict), 'No post-stage status')
                status = after['json']
                require(status.get('ready') is True and status.get('stale') is False, 'Post-stage status is stale')
                require(status.get('backend') == mode, 'Post-stage backend changed')
                require(status.get('records') == total - (stage == 'delete'), 'Post-stage record count differs')
                result['stage_status'][stage] = {key: status.get(key) for key in
                    ['reload_count', 'reload_attempts', 'incremental_applies', 'incremental_attempts',
                     'incremental_change_ids', 'last_incremental_change_ids', 'last_refresh_method',
                     'last_load_error', 'last_refresh_error', 'last_fallback_reason']}
                require(status.get('last_load_error') is None and status.get('last_refresh_error') is None,
                        'Service reported a refresh/load error')
                if mode == 'memory':
                    require(status.get('reload_count') == status.get('reload_attempts') == 0
                            and status.get('incremental_applies') == status.get('incremental_attempts') == stage_index
                            and status.get('incremental_change_ids') == stage_index
                            and status.get('last_incremental_change_ids') == (1 if stage_index else 0)
                            and status.get('last_refresh_method') == ('incremental' if stage_index else 'startup_full')
                            and status.get('last_fallback_reason') is None,
                            'Memory probe update did not remain incremental')
                else:
                    require(status.get('reload_count') == status.get('reload_attempts') == stage_index
                            and status.get('incremental_applies') == status.get('incremental_attempts') == 0
                            and status.get('last_refresh_method') == ('fallback_full' if stage_index else 'startup_full')
                            and status.get('last_fallback_reason') == ('backend_not_packed' if stage_index else None),
                            'SQLite probe update did not refresh exactly once')
        result['status'] = 'PASS'
    except Exception as error:
        result['error_type'] = type(error).__name__
        raw['failure'] = repr(error)
    finally:
        # A timed-out HTTP request can still be executing. Stop this service
        # before any other view is started; never overlap high-CPU queries.
        if sampler is not None:
            try:
                sampler.close()
            except Exception as error:
                result.update(status='FAIL', sampler_error_type=type(error).__name__)
                raw['sampler_failure'] = repr(error)
            raw['samples'] = sampler.samples
        if service is not None:
            try:
                raw['cleanup'] = close_owned(service, port)
                clean = raw['cleanup']
                result['cleanup_verified'] = clean['process_group_gone'] and clean['port_closed']
                result['cleanup'] = {key: clean[key] for key in
                    ['pid', 'returncode', 'method', 'process_group_gone', 'port_closed']}
                result['lifetime'] = {'cpu_seconds': clean['user_cpu_seconds'] + clean['system_cpu_seconds'],
                                      'process_peak_rss_bytes': clean['lifetime_peak_rss_bytes']}
                require(clean['process_group_gone'] and clean['port_closed'], 'Owned service group/port remained')
                require(clean['returncode'] == -15, 'Service did not survive to owned shutdown')
            except Exception as error:
                result.update(status='FAIL', cleanup_error_type=type(error).__name__)
                raw['cleanup_failure'] = repr(error)
        if mode == 'cli':
            result['cleanup_verified'] = all(phase.get('cleanup_verified') is True for phase in raw['phases'].values())
        expected = [query_key(stage, label) for stage in STAGES for label, _ in stage_queries(stage)]
        for key in expected:
            result['phases'].setdefault(key, {'status': 'NOT_RUN', 'reason': 'earlier_view_failure'})
        result['raw_reference'] = f'raw/{variant}-{mode}/raw.local.json'
        write_json(directory / 'raw.local.json', raw)
    return result, raw['phases']


def evaluate(views, outcomes):
    oracle = outcomes.get(('baseline', 'memory'), {})
    oracle_views = [view for view in views if view['variant'] == 'baseline' and view['mode'] == 'memory']
    expected_keys = {query_key(stage, label) for stage in STAGES for label, _ in stage_queries(stage)}
    identities = {(view['variant'], view['mode']) for view in views}
    required_identities = {(variant, mode) for variant in ['baseline', 'candidate'] for mode in ['cli', 'memory', 'sqlite']}
    gate = (len(views) == 6 and identities == required_identities and len(oracle_views) == 1
            and oracle_views[0]['status'] == 'PASS' and set(oracle) == expected_keys)
    comparisons = []
    for view in views:
        for key, phase in view['phases'].items():
            expected, current = oracle.get(key), outcomes[(view['variant'], view['mode'])].get(key)
            match = (expected is not None and current is not None and expected.get('status') == 'PASS'
                     and current.get('status') == 'PASS' and not expected.get('timed_out', False)
                     and not current.get('timed_out', False) and expected.get('json') == current.get('json'))
            phase['matches_full_index_oracle'] = match
            if view['variant'] == 'candidate' and not match:
                gate = False
            if view['variant'] == 'baseline' and view['mode'] == 'memory' and not match:
                gate = False
            if not match:
                comparisons.append({'variant': view['variant'], 'mode': view['mode'], 'phase': key,
                    'legacy_observation_only': view['variant'] == 'baseline' and view['mode'] != 'memory',
                    'outcome_status': phase['status']})
        if view['variant'] == 'candidate' and view['status'] != 'PASS':
            gate = False
        if set(view['phases']) != expected_keys:
            gate = False
        if view.get('cleanup_verified') is not True:
            gate = False
    return gate, comparisons


def resource_comparisons(views, outcomes):
    lookup = {(view['variant'], view['mode']): view for view in views}
    result = []
    for mode in ['cli', 'memory', 'sqlite']:
        baseline = lookup.get(('baseline', mode), {}).get('phases', {})
        candidate = lookup.get(('candidate', mode), {}).get('phases', {})
        for key in sorted(set(baseline) | set(candidate)):
            old, new = baseline.get(key, {}), candidate.get(key, {})
            old_raw = outcomes.get(('baseline', mode), {}).get(key, {})
            new_raw = outcomes.get(('candidate', mode), {}).get(key, {})
            same = old.get('status') == new.get('status') == 'PASS' and old_raw.get('json') == new_raw.get('json')
            item = {'mode': mode, 'phase': key, 'same_ordered_six_fields': same,
                    'baseline_status': old.get('status', 'NOT_RUN'), 'candidate_status': new.get('status', 'NOT_RUN'),
                    'baseline_timed_out': old.get('timed_out'), 'candidate_timed_out': new.get('timed_out'),
                    'baseline_exceeded_gui_15s': old.get('exceeded_gui_15s'),
                    'candidate_exceeded_gui_15s': new.get('exceeded_gui_15s')}
            for field in ['wall_seconds', 'cpu_seconds', 'end_rss_bytes', 'end_footprint_bytes',
                          'sampled_peak_rss_bytes', 'sampled_peak_footprint_bytes', 'process_peak_rss_bytes']:
                before, after = old.get('metrics', {}).get(field), new.get('metrics', {}).get(field)
                item[field] = {'baseline': before, 'candidate': after,
                               'change_percent': 100 * (after / before - 1) if before and after is not None else None}
            result.append(item)
    return result


def create_fixture(path, count):
    path = require_tmp(path)
    require(not path.exists() and 310 <= count <= 100000, 'Require a fresh fixture and 310..100000 rows')
    path.parent.mkdir(parents=True, exist_ok=True)
    with closing(sqlite3.connect(path)) as connection:
        connection.execute('CREATE TABLE files(id INTEGER PRIMARY KEY,path TEXT NOT NULL UNIQUE,path_lower TEXT,basename TEXT NOT NULL,basename_lower TEXT NOT NULL,ext_lower TEXT,kind TEXT NOT NULL,size INTEGER,mtime INTEGER,ctime INTEGER,dev INTEGER,inode INTEGER,indexed_at INTEGER NOT NULL)')
        for index in range(count):
            basename = 'code' if index < 300 else f'filler-code-{index:05d}.txt'
            value = f'/__topk_adversarial_fixture__/{index:06d}/' + basename
            modified = 0 if index < 300 else 1000000 + index
            connection.execute('INSERT INTO files VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)',
                (index + 1, value, value.lower(), basename, basename.lower(), None if index < 300 else 'txt',
                 'file', index, modified, 0, 1, index + 1, 1000))
        connection.commit()
    print(json.dumps({'fixture_created': True, 'rows': count, 'sha256': sha256(path),
                      'adversarial_candidate_cap_exercisable': count > 24300}))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    for name in ['baseline', 'candidate', 'snapshot', 'output']:
        parser.add_argument('--' + name, type=Path)
    parser.add_argument('--baseline-sha256', default=FROZEN_BASELINE_SHA)
    parser.add_argument('--candidate-sha256')
    parser.add_argument('--limit', type=int, default=300)
    parser.add_argument('--expect-code-count', type=int, default=300)
    parser.add_argument('--query-timeout', type=float, default=15)
    parser.add_argument('--startup-timeout', type=float, default=180)
    parser.add_argument('--sample-interval', type=float, default=.01)
    parser.add_argument('--skip-fuzzy', action='store_true', help='Explicitly omit only the real ext:rs fuzzy query; the six-probe fuzzy check remains mandatory')
    parser.add_argument('--prepare-compact', action='store_true', help='Explicitly prepare one candidate optimize copy for both versions, outside query timing; retain source/output DB evidence')
    parser.add_argument('--create-fixture', type=Path, help='Create a /tmp adversarial SQL fixture and exit without starting binaries')
    parser.add_argument('--fixture-rows', type=int, default=30000)
    args = parser.parse_args()
    if args.create_fixture:
        create_fixture(args.create_fixture, args.fixture_rows)
        return 0
    if args.skip_fuzzy:
        INITIAL_QUERIES[:] = [entry for entry in INITIAL_QUERIES if entry[0] != 'real_fuzzy']
    if (sys.platform != 'darwin' or any(getattr(args, key) is None for key in ['baseline', 'candidate', 'snapshot', 'output'])
            or args.limit < 1 or not 0 <= args.expect_code_count <= args.limit
            or not all(math.isfinite(v) for v in [args.query_timeout, args.startup_timeout, args.sample_interval])
            or args.query_timeout <= 0 or args.startup_timeout <= 0 or not .001 <= args.sample_interval <= 1):
        parser.error('Requires macOS, baseline/candidate/snapshot/output and positive finite limits')
    require(all(not contains_time_filter(query) for _, query in INITIAL_QUERIES), 'Time-dependent queries are not comparable')
    source = require_tmp(args.snapshot.resolve(strict=True))
    require(not nonempty_wal(source), 'Use a frozen snapshot without nonempty WAL')
    rollback = Path(str(source) + '-journal')
    require(not rollback.exists() or rollback.stat().st_size == 0, 'Source has an active rollback journal')
    output = require_tmp(args.output)
    require(not output.exists(), 'Fresh output required; preserve all earlier failures')
    output.mkdir(mode=0o700, parents=True)
    (output / 'raw').mkdir(mode=0o700)
    binaries = {key: getattr(args, key).resolve(strict=True) for key in ['baseline', 'candidate']}
    fingerprints = {key: identity(path, True) for key, path in binaries.items()}
    require(all(path.is_file() and os.access(path, os.X_OK) for path in binaries.values()), 'Require executable binaries')
    require(fingerprints['baseline']['sha256'] == args.baseline_sha256, 'Frozen baseline SHA mismatch')
    if args.candidate_sha256:
        require(fingerprints['candidate']['sha256'] == args.candidate_sha256, 'Candidate SHA mismatch')
    source_before, source_sha = db_identity(source), sha256(source)
    manifest = {'binary_identity': fingerprints, 'source_before': source_before,
                'arguments': {key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items()}}
    report = {'status': 'FAIL', 'scope': 'sequential same-snapshot exact TopK mechanism and resource comparison; no App/watch',
              'binary_sha256': {key: value['sha256'] for key, value in fingerprints.items()},
              'snapshot_sha256': source_sha, 'limit': args.limit, 'query_timeout_seconds': args.query_timeout,
              'fuzzy_probe_required': True, 'real_fuzzy_query_executed': not args.skip_fuzzy,
              'physical_preparation': {'requested': args.prepare_compact, 'status': 'NOT_STARTED' if args.prepare_compact else 'NOT_REQUESTED'},
              'query_sha256': {label: hashlib.sha256(query.encode()).hexdigest() for label, query in INITIAL_QUERIES},
              'views': [], 'notes': [
                  'Baseline packed memory is the per-stage full-index oracle; candidate all three paths must match.',
                  'Baseline CLI/sqlite mismatches and timeouts are retained legacy observations, not candidate exemptions.',
                  'Old bounded and new exact results can differ in workload; CPU comparisons are correction cost, not equal-recall speedup.',
                  'CLI CPU includes spawn/init/encoding; HTTP CPU includes refresh/encoding, with separate resident-load metrics.',
                  'Actual query timeout is explicit. A >15s completion does not meet the existing GUI timeout budget.',
                  'After an HTTP failure no later query is run on that process; owned cleanup precedes the next view.',
                  'All SQL mutations and migrations affect only owned /tmp clones, outside query timing.',
                  'Explicit physical compaction, when requested, is one shared preparation. Its cost is reported separately; this is not a code-only gain or cache-cold evidence.',
                  'With compaction requested, private raw preparation source/output DBs are deliberately retained, including failed or partial output; temporary view copies are still removed.',
                  'Six update probes contain no code text; source code300 match set is unchanged. Adversarial cap fixture is a separate input.',
                  'Samples may miss peaks; HWM is per-process wait4, not summed simultaneous memory.',
                  'One fixed sequential A/B, uncontrolled caches; no formal repeated performance conclusion.',
                  'Only summary is publishable; manifest/raw contain private paths and full results.',
              ]}
    scratch_name = None
    try:
        metrics = ProcessMetrics()
        with tempfile.TemporaryDirectory(prefix='macevery-topk-equivalence-', dir='/private/tmp') as scratch_name:
            scratch = Path(scratch_name)
            prepared = (output / 'raw' / 'prepared-source' / 'index.sqlite') if args.prepare_compact else (scratch / 'prepared' / 'index.sqlite')
            manifest['preparation_copy'] = clone_or_backup(source, prepared, args.startup_timeout)
            prep = run_cli(binaries['candidate'], prepared, ['status', '--json'], output / 'raw' / 'preparation',
                           args.startup_timeout, args.sample_interval, metrics)
            require(prep['status'] == 'PASS', 'Candidate preparation/migration failed')
            counts = {key: prep['json'][key] for key in ['files', 'dirs', 'apps', 'symlinks', 'others']}
            require(all(type(value) is int and value >= 0 for value in counts.values()), 'Invalid exact status counts')
            if args.prepare_compact:
                prepared = prepare_compacted_copy(binaries['candidate'], prepared,
                    output / 'raw' / 'prepared-compact' / 'index.sqlite', output / 'raw' / 'compaction',
                    args, metrics, sum(counts.values()), report['physical_preparation'])
            total = sum(counts.values()) + seed_probes(prepared)
            prepared_sha = sha256(prepared)
            outcomes = {}
            for variant in ['baseline', 'candidate']:
                for mode in ['cli', 'memory', 'sqlite']:
                    name = variant + '-' + mode
                    database = scratch / name / 'index.sqlite'
                    copied = clone_or_backup(prepared, database, args.startup_timeout)
                    directory = output / 'raw' / name
                    view, data = run_view(binaries[variant], variant, mode, database, directory, args, metrics, total)
                    write_json(directory / 'copy.local.json', copied)
                    report['views'].append(view)
                    outcomes[(variant, mode)] = data
                    require(view['cleanup_verified'], 'Owned process cleanup failed; do not start another view')
            require(sha256(prepared) == prepared_sha, 'Prepared shared input changed')
            gate, differences = evaluate(report['views'], outcomes)
            candidate_phases = [phase for view in report['views'] if view['variant'] == 'candidate'
                                for phase in view['phases'].values()]
            within_budget = all(phase.get('status') == 'PASS' and phase.get('timed_out') is False
                                and phase.get('exceeded_gui_15s') is False for phase in candidate_phases)
            report.update(exact_topk_gate_passed=gate, differences=differences,
                          candidate_all_queries_within_gui_15s=within_budget,
                          resource_comparisons=resource_comparisons(report['views'], outcomes),
                          status='PASS' if gate and within_budget else 'FAIL')
    except Exception as error:
        report['error_type'] = type(error).__name__
        (output / 'driver-failure.local.txt').write_text(repr(error) + '\n')
    finally:
        if scratch_name is not None:
            report['temporary_copies_removed'] = not Path(scratch_name).exists()
            if not report['temporary_copies_removed']:
                report.update(status='FAIL', temporary_cleanup_error_type='TemporaryCopiesRemain')
        if args.prepare_compact:
            report['retained_preparation_evidence'] = {
                'source_copy_present': (output / 'raw' / 'prepared-source' / 'index.sqlite').exists(),
                'output_copy_present': (output / 'raw' / 'prepared-compact' / 'index.sqlite').exists(),
                'private_raw_db_files_deliberately_retained': True,
            }
        try:
            report['source_unchanged'] = source_before == db_identity(source) and source_sha == sha256(source)
            report['binaries_unchanged'] = all(identity(path, True) == fingerprints[key] for key, path in binaries.items())
            if not report['source_unchanged'] or not report['binaries_unchanged']:
                report.update(status='FAIL', integrity_error_type='InputChanged')
        except Exception as error:
            report.update(status='FAIL', integrity_error_type=type(error).__name__)
        write_json(output / 'manifest.local.json', manifest)
        write_json(output / 'summary.json', report)
    print(json.dumps({'status': report['status'], 'exact_topk_gate_passed': report.get('exact_topk_gate_passed', False),
                      'views': len(report['views'])}))
    return 0 if report['status'] == 'PASS' else 1


if __name__ == '__main__':
    raise SystemExit(main())
