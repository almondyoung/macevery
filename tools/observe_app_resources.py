#!/usr/bin/env python3
"""Low-frequency, read-only observation of one frozen MacEvery App.

init --app APP --db DB --expected-source FULL_SHA --state NEW_DIR --hours 48
sample --state DIR             # one explicit endpoint (e.g. before/after sleep)
run --state DIR                # foreground; no App restart, wake lock or scheduler
summarize --state DIR          # read committed evidence only; no process/DB access

The default run interval is 60s. Hourly files are cumulative observations, not
proof of continuous coverage. Sleep, missed cadence, boot/identity changes,
collector restarts and unknown clocks break CPU intervals. Short-lived CLI
helpers and between-sample memory peaks may be missed. Raw/state files contain
private local paths; printed summaries contain only roles, counts and hashes.
"""
import argparse
from contextlib import contextmanager
import ctypes
from datetime import datetime, timezone
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import plistlib
import re
import signal
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from urllib.parse import quote
import uuid

from check_service_round2 import ProcessMetrics, RusageInfoV0


ROLES = ('gui', 'serve', 'watch')
SCHEMA = 1
MAX_RECORD_BYTES = 1024 * 1024
METRICS_HELPER = Path(__file__).with_name('check_service_round2.py')
# Conservatively discard even short sleep/clock-read jitter above 1 ms. Power
# UUID changes are separate barriers; they are not proof that sleep occurred.
SLEEP_DIFFERENCE_TOLERANCE_SECONDS = 0.001


class ObserverError(RuntimeError):
    pass


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def file_hash(path):
    h = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def utc(ns=None):
    return datetime.fromtimestamp((time.time_ns() if ns is None else ns) / 1e9,
                                 timezone.utc).isoformat()


def file_stat(path):
    s = Path(path).stat()
    return {'device': s.st_dev, 'inode': s.st_ino, 'bytes': s.st_size, 'mtime_ns': s.st_mtime_ns}


def app_fingerprint(app, expected_source):
    app = Path(app).resolve(strict=True)
    plist = app / 'Contents/Info.plist'
    info = plistlib.loads(plist.read_bytes())
    source = info.get('MacEveryGitCommit')
    if source != expected_source:
        raise ObserverError('App source differs from the explicitly frozen source')
    files = {'plist': plist, 'gui': app / 'Contents/MacOS/MacEveryApp',
             'cli': app / 'Contents/MacOS/macevery'}
    hashes = {role: file_hash(path) for role, path in files.items()}
    return {'source_commit': source, 'build_label': info.get('MacEveryBuildLabel'),
            'hashes': hashes, 'fingerprint': digest({'source': source, 'hashes': hashes}),
            'file_stats': {role: file_stat(path) for role, path in files.items()}}


def atomic_json(path, value):
    """Replace a cache/state atomically; no partial JSON becomes visible."""
    path = Path(path)
    fd, temporary = tempfile.mkstemp(prefix='.' + path.name + '-', dir=path.parent)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, 'w') as stream:
            json.dump(value, stream, ensure_ascii=False, separators=(',', ':'))
            stream.write('\n')
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def append_record(path, record):
    """One writer, one append+fsync. Partial tails are retained, never repaired."""
    data = (json.dumps(record, ensure_ascii=False, separators=(',', ':')) + '\n').encode()
    if len(data) > MAX_RECORD_BYTES:
        raise ObserverError('Observation exceeded its evidence size limit')
    fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
    try:
        if os.write(fd, data) != len(data):
            raise ObserverError('Partial journal append; preserve this state for diagnosis')
        os.fsync(fd)
        return os.fstat(fd).st_size
    finally:
        os.close(fd)


@contextmanager
def writer_lock(directory):
    directory = Path(directory)
    if directory.stat().st_mode & 0o077:
        raise ObserverError('State directory must have mode 0700')
    fd = os.open(directory / 'writer.lock', os.O_RDWR | os.O_CREAT, 0o600)
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise ObserverError('Another observation writer is active') from error
        yield
    finally:
        os.close(fd)


def read_records(directory, limit=None):
    path = Path(directory) / 'samples.jsonl'
    if not path.exists():
        return []
    with path.open('rb') as stream:
        data = stream.read() if limit is None else stream.read(limit)
    if data and not data.endswith(b'\n'):
        raise ObserverError('Incomplete JSONL tail; evidence is preserved, automatic continuation refused')
    records = []
    for line in data.splitlines():
        row = json.loads(line)
        if row['sequence'] != len(records) + 1:
            raise ObserverError('JSONL sequence mismatch')
        records.append(row)
    return records


def load_state(directory, verify_tool=True):
    directory = Path(directory)
    manifest = json.loads((directory / 'manifest.local.json').read_text())
    state = json.loads((directory / 'state.json').read_text())
    if manifest.get('schema') != SCHEMA:
        raise ObserverError('Unsupported observation schema')
    if verify_tool and manifest['tool_sha256'] != file_hash(__file__):
        raise ObserverError('Observer source changed; use a new state directory')
    if verify_tool and manifest['metrics_helper_sha256'] != file_hash(METRICS_HELPER):
        raise ObserverError('Observer metrics helper changed; use a new state directory')
    actual = (directory / 'samples.jsonl').stat().st_size
    if actual < state['journal_bytes']:
        raise ObserverError('Committed JSONL history was truncated; preserve this state for diagnosis')
    if actual > state['journal_bytes']:
        # Append succeeded but the process may have stopped before state replace.
        records = read_records(directory)
        state['sequence'] = len(records)
        state['last_sample'] = records[-1] if records else None
        state['journal_bytes'] = actual
        state['recovered_append_before_state'] = True
        if records and records[-1].get('deadline_reached'):
            state['window_finished'] = True
    return manifest, state


class NativeProbe:
    def __init__(self):
        if sys.platform != 'darwin':
            raise ObserverError('Native observation requires macOS')
        self.metrics = ProcessMetrics()
        self.lib = self.metrics.library
        self.lib.proc_pidpath.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_uint32]
        self.lib.proc_pidpath.restype = ctypes.c_int
        self.system = ctypes.CDLL('/usr/lib/libSystem.B.dylib')
        for name in ['mach_absolute_time', 'mach_continuous_time']:
            fn = getattr(self.system, name)
            fn.argtypes = []
            fn.restype = ctypes.c_uint64

    def clocks(self):
        value = {'boot_uuid': None, 'sleep_wake_uuid': None,
                 'awake_ns': None, 'continuous_ns': None}
        try:
            value['boot_uuid'] = subprocess.check_output(
                ['/usr/sbin/sysctl', '-n', 'kern.bootsessionuuid'], text=True, timeout=2).strip() or None
        except (OSError, subprocess.SubprocessError):
            value['boot_error'] = 'boot_uuid_unavailable'
        try:
            output = subprocess.check_output(['/usr/bin/pmset', '-g', 'uuid'], text=True, timeout=2)
            identifiers = set(re.findall(r'\b[0-9a-fA-F]{8}-(?:[0-9a-fA-F]{4}-){3}[0-9a-fA-F]{12}\b', output))
            if len(identifiers) != 1:
                raise ObserverError('Power transition UUID unavailable')
            value['sleep_wake_uuid'] = identifiers.pop().lower()
        except (OSError, subprocess.SubprocessError, ObserverError):
            value['power_error'] = 'sleep_wake_uuid_unavailable'
        value['wall_ns'] = time.time_ns()
        value['utc'] = utc(value['wall_ns'])
        try:
            numer, denom = self.metrics.timebase.numer, self.metrics.timebase.denom
            value['awake_ns'] = self.system.mach_absolute_time() * numer // denom
            value['continuous_ns'] = self.system.mach_continuous_time() * numer // denom
        except (AttributeError, OSError, ZeroDivisionError):
            value['clock_error'] = 'mach_clocks_unavailable'
        return value

    def usage(self, pid):
        info = RusageInfoV0()
        if self.metrics.query(pid, 0, ctypes.byref(info)):
            raise ObserverError('proc_pid_rusage unavailable')
        if not info.proc_start_abstime:
            raise ObserverError('Process startup cookie unavailable')
        ratio = self.metrics.timebase
        return {'start_abstime': info.proc_start_abstime, 'uuid': bytes(info.uuid).hex(),
                'cpu_ns': (info.user_time * ratio.numer // ratio.denom
                           + info.system_time * ratio.numer // ratio.denom),
                'rss_bytes': info.resident_size, 'footprint_bytes': info.phys_footprint}

    def executable(self, pid):
        buffer = ctypes.create_string_buffer(4096)
        if self.lib.proc_pidpath(pid, buffer, len(buffer)) <= 0:
            raise ObserverError('proc_pidpath unavailable')
        return os.fsdecode(buffer.value)

    def processes(self, app, clock):
        app = Path(app)
        gui = str(app / 'Contents/MacOS/MacEveryApp')
        cli = str(app / 'Contents/MacOS/macevery')
        rows, unknown = {role: [] for role in ROLES}, []
        inventory = subprocess.check_output(['/bin/ps', '-ww', '-axo', 'pid=,ppid=,comm='],
                                            text=True, errors='replace', timeout=3)
        for line in inventory.splitlines():
            fields = line.split(None, 2)
            if len(fields) != 3 or fields[2] not in (gui, cli):
                continue
            pid, parent = int(fields[0]), int(fields[1])
            role = 'gui' if fields[2] == gui else None
            try:
                before = self.usage(pid)
                executable = self.executable(pid)
                if executable != fields[2]:
                    raise ObserverError('Executable changed during discovery')
                if executable == gui:
                    role = 'gui'
                else:
                    command = subprocess.check_output(['/bin/ps', '-ww', '-p', str(pid), '-o', 'command='],
                                                      text=True, errors='replace', timeout=2).strip()
                    # Never persist command arguments or environment (tokens).
                    if command.startswith(cli + ' '):
                        operation = command[len(cli):].strip().split(None, 1)[0]
                        role = operation if operation in ('serve', 'watch') else None
                after = self.usage(pid)
                if ((before['start_abstime'], before['uuid']) != (after['start_abstime'], after['uuid'])
                        or self.executable(pid) != executable):
                    raise ObserverError('Process identity changed during capture')
                if role is None:
                    unknown.append({'pid': pid, 'reason': 'untracked_short_lived_or_other_cli'})
                    continue
                known = bool(clock.get('boot_uuid'))
                identity = {'boot_uuid': clock.get('boot_uuid'), 'pid': pid,
                            'start_abstime': after['start_abstime'], 'uuid': after['uuid'],
                            'executable': executable}
                rows[role].append({'status': 'known' if known else 'unknown', 'pid': pid,
                                   'parent': parent, 'identity': identity,
                                   'identity_key': digest(identity) if known else None, **after})
            except (OSError, subprocess.SubprocessError, ObserverError, ValueError) as error:
                item = {'status': 'unknown', 'pid': pid, 'parent': parent,
                        'role': role, 'reason': type(error).__name__}
                if role is not None:
                    rows[role].append(item)
                else:
                    unknown.append(item)
        result = {}
        for role, items in rows.items():
            unresolved = role != 'gui' and any(item.get('status') == 'unknown' for item in unknown)
            # A failed same-bundle CLI capture may be another serve/watch. It
            # cannot be silently discarded when claiming one unique owner.
            result[role] = ({'status': 'unknown', 'reason': 'unresolved_cli_identity',
                             'candidate_pids': [item['pid'] for item in items]} if unresolved else
                            items[0] if len(items) == 1 else
                            {'status': 'missing' if not items else 'ambiguous',
                             'candidate_pids': [item['pid'] for item in items]})
        owner = result['gui']
        for role in ('serve', 'watch'):
            row = result[role]
            if row.get('status') == 'known' and (owner.get('status') != 'known'
                                                or row['parent'] != owner['pid']):
                row['status'] = 'unknown'
                row['reason'] = 'gui_parent_not_verified'
                row['identity_key'] = None
        return result, unknown


def database_meta(path):
    """Read only singleton/control metadata. No files COUNT/search/schema DDL."""
    before = file_stat(path)
    connection = sqlite3.connect('file:' + quote(str(path), safe='/') + '?mode=ro',
                                 uri=True, timeout=0.2)
    try:
        deadline = time.monotonic() + 1
        connection.set_progress_handler(lambda: time.monotonic() > deadline, 100)
        connection.execute('BEGIN')
        row = connection.execute('SELECT database_id,revision,journal_floor FROM macevery_content_state WHERE singleton=1').fetchone()
        last = connection.execute("SELECT value FROM meta WHERE key='last_indexed_at'").fetchone()
        connection.commit()
        if (row is None or not isinstance(row[0], str) or not row[0]
                or any(type(v) is not int for v in row[1:])):
            raise ObserverError('Invalid content singleton')
        after = file_stat(path)
        if (before['device'], before['inode']) != (after['device'], after['inode']):
            raise ObserverError('Database identity changed while reading')
        return {'status': 'known', 'file_identity': {k: after[k] for k in ('device', 'inode')},
                'database_id': row[0], 'revision': row[1], 'journal_floor': row[2],
                'last_indexed_at_text': last[0] if last else None}
    finally:
        connection.close()


def read_database_meta(path):
    try:
        return database_meta(path)
    except (OSError, sqlite3.Error, ObserverError) as error:
        return {'status': 'unknown', 'error': type(error).__name__}


def database_pin_status(initial, current):
    if initial.get('status') != 'known' or current.get('status') != 'known':
        return 'unknown'
    keys = ('database_id', 'file_identity')
    return 'match' if all(initial[k] == current[k] for k in keys) else 'mismatch'


def elapsed_ns(manifest, clock):
    start = manifest['initial_clock']
    if (clock.get('boot_uuid') and clock['boot_uuid'] == start.get('boot_uuid')
            and clock.get('continuous_ns') is not None and start.get('continuous_ns') is not None):
        return max(0, clock['continuous_ns'] - start['continuous_ns']), 'continuous_same_boot'
    return max(0, clock['wall_ns'] - start['wall_ns']), 'wall_fallback_unknown_continuity'


def interval_between(previous, current, cadence):
    out = {'barriers': [], 'roles': {}, 'awake_seconds': None, 'sleep_seconds': None}
    if previous is None:
        out['barriers'].append('initial_endpoint')
        return out
    a, b = previous['clock'], current['clock']
    barriers = out['barriers']
    if not a.get('boot_uuid') or not b.get('boot_uuid'):
        barriers.append('boot_unknown')
    elif a['boot_uuid'] != b['boot_uuid']:
        barriers.append('boot_changed')
    if previous['collector_session'] != current['collector_session']:
        barriers.append('collector_session_changed')
    if not a.get('sleep_wake_uuid') or not b.get('sleep_wake_uuid'):
        barriers.append('power_state_unknown')
    elif a['sleep_wake_uuid'] != b['sleep_wake_uuid']:
        barriers.append('power_transition')
    if any(x.get(k) is None for x in (a, b) for k in ('awake_ns', 'continuous_ns')):
        barriers.append('clock_unknown')
    elif not any(v in barriers for v in ('boot_unknown', 'boot_changed')):
        awake = (b['awake_ns'] - a['awake_ns']) / 1e9
        continuous = (b['continuous_ns'] - a['continuous_ns']) / 1e9
        out.update(awake_seconds=awake, elapsed_seconds=continuous,
                   sleep_seconds=max(0, continuous - awake))
        if awake <= 0 or continuous <= 0 or continuous + SLEEP_DIFFERENCE_TOLERANCE_SECONDS < awake:
            barriers.append('clock_non_monotonic')
        if continuous - awake > SLEEP_DIFFERENCE_TOLERANCE_SECONDS:
            barriers.append('sleep_observed')
        if continuous > cadence * 1.5 + 5:
            barriers.append('sampling_gap')
        if abs((b['wall_ns'] - a['wall_ns']) / 1e9 - continuous) > 5:
            barriers.append('wall_clock_adjustment')
    if previous.get('source_status') != 'match' or current.get('source_status') != 'match':
        barriers.append('source_unverified')
    if previous.get('database_pin_status') != 'match' or current.get('database_pin_status') != 'match':
        barriers.append('database_identity_unverified')
    if previous.get('capture_barriers') or current.get('capture_barriers'):
        barriers.append('endpoint_capture_unverified')
    for role in ROLES:
        before, after = previous['roles'].get(role, {}), current['roles'].get(role, {})
        reason = None
        if before.get('status') != 'known' or after.get('status') != 'known':
            reason = 'runtime_unknown_or_missing'
        elif not before.get('identity_key') or before['identity_key'] != after.get('identity_key'):
            reason = 'process_identity_changed'
        elif after.get('cpu_ns') is None or before.get('cpu_ns') is None:
            reason = 'cpu_unknown'
        elif after['cpu_ns'] < before['cpu_ns']:
            reason = 'negative_cpu_delta'
        if not barriers and reason is None:
            cpu = (after['cpu_ns'] - before['cpu_ns']) / 1e9
            out['roles'][role] = {'status': 'measured', 'cpu_seconds': cpu,
                                  'awake_seconds': out['awake_seconds'],
                                  'average_cpu_percent_one_core': cpu / out['awake_seconds'] * 100}
        else:
            out['roles'][role] = {'status': 'unknown', 'reason': reason or 'interval_barrier'}
    return out


def capture_barriers(before, after, cadence):
    """A sleep/clock change inside capture must not shift a CPU endpoint."""
    if not before.get('boot_uuid') or before.get('boot_uuid') != after.get('boot_uuid'):
        return ['capture_boot_unverified']
    if any(c.get(k) is None for c in (before, after) for k in ('awake_ns', 'continuous_ns')):
        return ['capture_clock_unknown']
    reasons = []
    if not before.get('sleep_wake_uuid') or not after.get('sleep_wake_uuid'):
        reasons.append('capture_power_state_unknown')
    elif before['sleep_wake_uuid'] != after['sleep_wake_uuid']:
        reasons.append('capture_power_transition')
    awake = (after['awake_ns'] - before['awake_ns']) / 1e9
    elapsed = (after['continuous_ns'] - before['continuous_ns']) / 1e9
    if awake < 0 or elapsed < 0 or elapsed + SLEEP_DIFFERENCE_TOLERANCE_SECONDS < awake:
        reasons.append('capture_clock_non_monotonic')
    if elapsed - awake > SLEEP_DIFFERENCE_TOLERANCE_SECONDS:
        reasons.append('capture_sleep_observed')
    if elapsed > cadence * 1.5 + 5:
        reasons.append('capture_delay')
    if abs((after['wall_ns'] - before['wall_ns']) / 1e9 - elapsed) > 5:
        reasons.append('capture_wall_clock_adjustment')
    return reasons


def summarize_records(manifest, records, state=None):
    state = state or {}
    valid = [r for r in records if r.get('in_window', True)]
    duration = manifest['duration_seconds']
    elapsed = min(duration, valid[-1]['elapsed_seconds']) if valid else 0
    latest = records[-1] if records else None
    summary = {'schema': SCHEMA, 'source_commit': manifest['expected_source'],
               'app_fingerprint': manifest['app_identity']['fingerprint'],
               'window_hours': duration / 3600, 'window_finished': state.get('window_finished', False),
               'elapsed_window_seconds': elapsed, 'samples': len(valid),
               'last_sample_utc': valid[-1]['clock']['utc'] if valid else None,
               'nominal_interval_seconds': manifest['interval_seconds'],
               'sleep_clock_difference_tolerance_seconds': SLEEP_DIFFERENCE_TOLERANCE_SECONDS,
               'nominal_slots_so_far': math.floor(elapsed / manifest['interval_seconds']) + 1,
               'captured_unique_slots': len({int(min(duration, r['elapsed_seconds']) // manifest['interval_seconds']) for r in valid}),
               'deadline_evaluation_clock_basis': latest['elapsed_clock_basis'] if latest else None,
               'last_endpoint_elapsed_seconds': latest['elapsed_seconds'] if latest else None,
               'last_endpoint_age_wall_seconds': max(0, (time.time_ns() - latest['clock']['wall_ns']) / 1e9) if latest else None,
               'barrier_counts': {}, 'database_pin_status_counts': {}, 'roles': {},
               'endpoints_with_untracked_cli': sum(bool(r.get('untracked_processes')) for r in valid),
               'endpoints_with_unresolved_cli': sum(any(p.get('status') == 'unknown' for p in r.get('untracked_processes', [])) for r in valid),
               'initial_database_status': manifest['initial_database'].get('status'),
               'last_database_pin_status': latest.get('database_pin_status') if latest else None,
               'worker': state.get('worker'),
               'last_exit': state.get('last_exit'),
               'notes': ['Discrete sampled RSS/footprint peaks may miss between-sample peaks.',
                         'CPU only joins adjacent same-identity samples without sleep/gap/collector barriers.',
                         'No continuous coverage/idle/leak PASS is inferred from the window ending.',
                         'Short-lived CLI helper CPU can be missed. The three roles are reported separately.',
                         'Worker status is last persisted bookkeeping, not a live PID check.',
                         'A final endpoint can straddle the deadline; exact measured interval lengths remain recorded.',
                         'CPU, RSS and footprint are segmented by observed process identity; a restart is not a leak trend.',
                         'Hourly files are cumulative as-of snapshots; missing hours are not backfilled.']}
    summary['notes'].append('Power UUID changes are conservative interval barriers, not independent proof of actual sleep; clock differences above 1ms are also excluded.')
    for row in valid:
        pin = row.get('database_pin_status', 'unknown')
        summary['database_pin_status_counts'][pin] = summary['database_pin_status_counts'].get(pin, 0) + 1
        for barrier in row['interval']['barriers']:
            summary['barrier_counts'][barrier] = summary['barrier_counts'].get(barrier, 0) + 1
    for role in ROLES:
        points = [r['roles'][role] for r in valid if r['roles'].get(role, {}).get('status') == 'known']
        spans = [r['interval']['roles'][role] for r in valid
                 if r['interval']['roles'].get(role, {}).get('status') == 'measured']
        seconds = sum(s['awake_seconds'] for s in spans)
        cpu = sum(s['cpu_seconds'] for s in spans)
        identities = list(dict.fromkeys(x['identity_key'] for x in points))
        summary['roles'][role] = {
            'known_endpoints': len(points), 'unknown_endpoints': len(valid) - len(points),
            'observed_identity_count': len(identities),
            'minimum_observed_restarts': max(0, len(identities) - 1),
            'measured_intervals': len(spans), 'measured_awake_seconds': seconds,
            'cpu_seconds_in_measured_intervals': cpu,
            'average_cpu_percent_one_core_in_measured_intervals': cpu / seconds * 100 if seconds else None,
            'rss_first_bytes': points[0]['rss_bytes'] if points else None,
            'rss_last_bytes': points[-1]['rss_bytes'] if points else None,
            'sampled_peak_rss_bytes': max((x['rss_bytes'] for x in points), default=None),
            'footprint_first_bytes': points[0]['footprint_bytes'] if points else None,
            'footprint_last_bytes': points[-1]['footprint_bytes'] if points else None,
            'sampled_peak_footprint_bytes': max((x['footprint_bytes'] for x in points), default=None)}
        segments = []
        for identity in identities:
            endpoints = [r for r in valid if r['roles'].get(role, {}).get('identity_key') == identity
                         and r['roles'][role].get('status') == 'known']
            spans = [r['interval']['roles'][role] for r in endpoints
                     if r['interval']['roles'].get(role, {}).get('status') == 'measured']
            measured = sum(s['awake_seconds'] for s in spans)
            cpu_seconds = sum(s['cpu_seconds'] for s in spans)
            first, last = endpoints[0], endpoints[-1]
            segments.append({'identity_key': identity, 'pid': first['roles'][role]['pid'],
                             'first_utc': first['clock']['utc'], 'last_utc': last['clock']['utc'],
                             'endpoints': len(endpoints), 'measured_awake_seconds': measured,
                             'cpu_seconds_in_measured_intervals': cpu_seconds,
                             'average_cpu_percent_one_core': cpu_seconds / measured * 100 if measured else None,
                             'rss_first_bytes': first['roles'][role]['rss_bytes'],
                             'rss_last_bytes': last['roles'][role]['rss_bytes'],
                             'sampled_peak_rss_bytes': max(r['roles'][role]['rss_bytes'] for r in endpoints),
                             'footprint_first_bytes': first['roles'][role]['footprint_bytes'],
                             'footprint_last_bytes': last['roles'][role]['footprint_bytes']})
        summary['roles'][role]['process_identities'] = segments
    return summary


def initialize(directory, app, database, expected_source, hours=48, interval=60, probe=None):
    if not re.fullmatch(r'[0-9a-f]{40}', expected_source):
        raise ObserverError('Expected source must be a full lowercase commit SHA')
    if (not math.isfinite(hours) or not math.isfinite(interval)
            or not 0 < hours <= 48 or not 10 <= interval <= 3600):
        raise ObserverError('Require 0<hours<=48 and 10<=interval<=3600 seconds')
    identity = app_fingerprint(app, expected_source)
    probe = probe or NativeProbe()
    clock = probe.clocks()
    initial_database = read_database_meta(Path(database).resolve())
    directory = Path(directory)
    directory.mkdir(mode=0o700, parents=True, exist_ok=False)
    manifest = {'schema': SCHEMA, 'app': str(Path(app).resolve()), 'database': str(Path(database).resolve()),
                'expected_source': expected_source, 'app_identity': identity, 'initial_clock': clock,
                'initial_database': initial_database,
                'duration_seconds': hours * 3600, 'interval_seconds': interval,
                'deadline_utc': utc(clock['wall_ns'] + int(hours * 3600 * 1e9)),
                'manual_session': 'manual-' + uuid.uuid4().hex,
                'tool_sha256': file_hash(__file__),
                'metrics_helper_sha256': file_hash(METRICS_HELPER)}
    state = {'sequence': 0, 'journal_bytes': 0, 'last_sample': None, 'window_finished': False,
             'last_summary_hour': -1, 'worker': None, 'last_exit': None}
    with writer_lock(directory):
        (directory / 'samples.jsonl').touch(mode=0o600, exist_ok=False)
        (directory / 'hourly').mkdir(mode=0o700)
        atomic_json(directory / 'manifest.local.json', manifest)
        atomic_json(directory / 'state.json', state)
        atomic_json(directory / 'summary.json', summarize_records(manifest, [], state))
    return manifest, state


def capture(directory, manifest, state, probe, session, mode='sample'):
    if state.get('window_finished'):
        raise ObserverError('Observation window already finished; use a new state directory')
    try:
        clock = probe.clocks()
    except (OSError, ObserverError):
        clock = {'wall_ns': time.time_ns(), 'utc': utc(), 'boot_uuid': None,
                 'awake_ns': None, 'continuous_ns': None, 'clock_error': 'capture_failed'}
    elapsed, clock_basis = elapsed_ns(manifest, clock)
    row = {'schema': SCHEMA, 'sequence': state['sequence'] + 1, 'clock': clock,
           'elapsed_seconds': elapsed / 1e9, 'elapsed_clock_basis': clock_basis,
           'collector_session': session, 'mode': mode, 'source_status': 'unknown'}
    try:
        current = app_fingerprint(manifest['app'], manifest['expected_source'])
        row['source_status'] = ('match' if current['fingerprint'] == manifest['app_identity']['fingerprint']
                                else 'mismatch')
    except ObserverError as error:
        row.update(source_status='mismatch', source_error=type(error).__name__)
    except (OSError, ValueError, plistlib.InvalidFileException) as error:
        row['source_error'] = type(error).__name__
    if row['source_status'] == 'match':
        try:
            row['roles'], row['untracked_processes'] = probe.processes(manifest['app'], clock)
        except (OSError, subprocess.SubprocessError, ObserverError) as error:
            row['runtime_error'] = type(error).__name__
    row.setdefault('roles', {role: {'status': 'unknown'} for role in ROLES})
    row['database'] = read_database_meta(manifest['database'])
    row['database_pin_status'] = database_pin_status(manifest['initial_database'], row['database'])
    try:
        row['capture_finished_clock'] = probe.clocks()
        row['capture_barriers'] = capture_barriers(clock, row['capture_finished_clock'], manifest['interval_seconds'])
    except (OSError, ObserverError):
        row['capture_barriers'] = ['capture_clock_unknown']
    row['capture_finished_utc'] = utc()
    row['deadline_reached'] = elapsed >= manifest['duration_seconds'] * 1e9
    # One final endpoint may straddle the deadline by one normal cadence. It
    # remains explicit; no later independent sampling extends the closed window.
    row['in_window'] = elapsed <= (manifest['duration_seconds'] + manifest['interval_seconds']) * 1e9
    row['interval'] = interval_between(state['last_sample'], row, manifest['interval_seconds'])
    old_db = (state.get('last_sample') or {}).get('database', manifest['initial_database'])
    new_db = row['database']
    row['database_events'] = []
    if row['database_pin_status'] != 'match':
        row['database_events'].append('initial_database_pin_' + row['database_pin_status'])
    if old_db.get('status') == 'known' and new_db.get('status') == 'known':
        if (old_db['database_id'], old_db['file_identity']) != (new_db['database_id'], new_db['file_identity']):
            row['database_events'].append('database_identity_changed')
        elif new_db['revision'] < old_db['revision']:
            row['database_events'].append('content_revision_decreased')
        else:
            row['content_revision_delta'] = new_db['revision'] - old_db['revision']
    state['journal_bytes'] = append_record(Path(directory) / 'samples.jsonl', row)
    state.update(sequence=row['sequence'], last_sample=row,
                 window_finished=row['deadline_reached'])
    atomic_json(Path(directory) / 'state.json', state)
    return row


def save_summary(directory, manifest, state, force=False):
    directory = Path(directory)
    records = read_records(directory, state['journal_bytes'])
    report = summarize_records(manifest, records, state)
    hour = int(report['elapsed_window_seconds'] // 3600)
    atomic_json(directory / 'summary.json', report)
    if force or hour > state['last_summary_hour']:
        path = directory / 'hourly' / f'hour-{hour:03d}-sequence-{state["sequence"]:06d}.json'
        atomic_json(path, report)
        state['last_summary_hour'] = hour
        atomic_json(directory / 'state.json', state)
    return report


def run_observer(directory, probe=None):
    directory = Path(directory)
    probe = probe or NativeProbe()
    stopped = threading.Event()
    previous_handlers = {}
    for sig in (signal.SIGINT, signal.SIGTERM):
        previous_handlers[sig] = signal.signal(sig, lambda *_: stopped.set())
    try:
        with writer_lock(directory):
            manifest, state = load_state(directory)
            session = 'run-' + uuid.uuid4().hex
            try:
                startup = probe.usage(os.getpid())
                worker_cookie = startup['start_abstime']
            except (AttributeError, OSError, ObserverError):
                worker_cookie = None
            state['worker'] = {'pid': os.getpid(), 'start_abstime': worker_cookie,
                               'collector_session': session, 'started_utc': utc(), 'status': 'running'}
            atomic_json(directory / 'state.json', state)
            reason, exit_code = 'window_elapsed', 0
            try:
                while not state['window_finished'] and not stopped.is_set():
                    row = capture(directory, manifest, state, probe, session, 'run')
                    hour = int(min(row['elapsed_seconds'], manifest['duration_seconds']) // 3600)
                    if hour > state['last_summary_hour'] or state['window_finished']:
                        save_summary(directory, manifest, state)
                    if row['source_status'] == 'mismatch':
                        raise ObserverError('Frozen App fingerprint changed; observer stopped')
                    if row['database_pin_status'] == 'mismatch':
                        raise ObserverError('Frozen database identity changed; observer stopped')
                    if state['window_finished']:
                        break
                    # Anchor cadence and end to init's absolute window, not
                    # capture duration or this worker's (possibly resumed) start.
                    now_elapsed = elapsed_ns(manifest, probe.clocks())[0] / 1e9
                    next_slot = (math.floor(now_elapsed / manifest['interval_seconds']) + 1) * manifest['interval_seconds']
                    until = min(next_slot, manifest['duration_seconds']) - now_elapsed
                    stopped.wait(max(0, until))
                if stopped.is_set():
                    reason = 'requested_signal_stop'
            except BaseException as error:
                reason, exit_code = type(error).__name__, 1
                raise
            finally:
                state['worker']['status'] = 'exited'
                state['last_exit'] = {'at_utc': utc(), 'reason': reason, 'exit_code': exit_code,
                                      'sequence': state['sequence']}
                atomic_json(directory / 'state.json', state)
                save_summary(directory, manifest, state, force=True)
    finally:
        for sig, handler in previous_handlers.items():
            signal.signal(sig, handler)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest='mode', required=True)
    initial = commands.add_parser('init')
    initial.add_argument('--app', type=Path, required=True)
    initial.add_argument('--db', type=Path, required=True)
    initial.add_argument('--expected-source', required=True)
    initial.add_argument('--hours', type=float, default=48)
    initial.add_argument('--interval', type=float, default=60)
    for p in [initial] + [commands.add_parser(name) for name in ('sample', 'run', 'summarize')]:
        p.add_argument('--state', type=Path, required=True)
    args = parser.parse_args()
    try:
        if args.mode == 'init':
            manifest, state = initialize(args.state, args.app, args.db, args.expected_source,
                                         args.hours, args.interval)
            print(json.dumps({'initialized': True, 'deadline_utc': manifest['deadline_utc'],
                              'source_commit': manifest['expected_source'],
                              'initial_database_status': manifest['initial_database']['status'], 'started': False}))
        elif args.mode == 'run':
            run_observer(args.state)
            print(json.dumps({'observer_exited': True}))
        elif args.mode == 'sample':
            with writer_lock(args.state):
                manifest, state = load_state(args.state)
                row = capture(args.state, manifest, state, NativeProbe(), manifest['manual_session'])
                report = save_summary(args.state, manifest, state)
                print(json.dumps({'sample_sequence': row['sequence'], 'source_status': row['source_status'],
                                  'database_pin_status': row['database_pin_status'],
                                  'interval_barriers': row['interval']['barriers'],
                                  'window_finished': report['window_finished']}))
                if row['source_status'] == 'mismatch' or row['database_pin_status'] == 'mismatch':
                    return 1
        else:
            # Atomic state defines the committed journal prefix. No flock,
            # process query, database access or write: heartbeat can use this
            # while a foreground run holds the exclusive writer lock.
            manifest = json.loads((args.state / 'manifest.local.json').read_text())
            state = json.loads((args.state / 'state.json').read_text())
            report = summarize_records(manifest, read_records(args.state, state['journal_bytes']), state)
            print(json.dumps(report, indent=2))
        return 0
    except (OSError, ValueError, sqlite3.Error, ObserverError) as error:
        # Do not emit private paths or raw command/DB text to a scheduler log.
        print(json.dumps({'status': 'ERROR', 'error_type': type(error).__name__,
                          'details': str(error) if isinstance(error, ObserverError) else 'See local evidence/state'}),
              file=sys.stderr)
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
