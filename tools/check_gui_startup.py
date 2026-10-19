#!/usr/bin/env python3
"""Run the production Swift model against owned real CLI/HTTP processes.

Compile only (safe while preparing a separately scheduled integration run):
  python3 tools/check_gui_startup.py --expected-source FULL_CANDIDATE_COMMIT_SHA \
    --compile-only --output /tmp/gui-startup-build

Root schedules the real functional run separately from performance measurements:
  python3 tools/check_gui_startup.py --expected-source FULL_CANDIDATE_COMMIT_SHA \
    --binary /tmp/frozen/macevery \
    --snapshot /tmp/frozen/index.sqlite --harness /tmp/gui-startup-build/checks \
    --output /tmp/gui-startup-integration

Only /tmp frozen inputs/copies are accepted. No installed App, watcher, real
17649 endpoint or live DB is used. The Swift URLProtocol intercepts all model
requests and forwards only to an authenticated owned random loopback port.
Two timeout checks SIGSTOP only that owned service, exercising the real 15s
URLSession timeout and real CLI fallback; the cancellation check additionally
holds only its owned CLI child. Full HTTP/CLI results stay in local raw files.
Independent CLI/HTTP oracles must have exactly the same ordered six-field
results. The preflight memory-oracle service is fully closed before cold start.

summary.json is sanitized: hashes/counts/metrics/cleanup, no results, paths or
auth token. CPU receipts use unique wait4 owners; wrapper/controller overhead
is distinguished from the actual Rust child. This is functional integration,
not a window first-frame, native App, TCC, FSEvents or CPU A/B/A benchmark.
"""
import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import signal
import subprocess
import sys
import threading
import time

from check_service_round1 import json_digest
from check_service_round2 import (
    OwnedService, RusageInfoV0, clone_or_backup, db_identity, free_port, require,
    require_tmp, request, response_json, sha256, write_json,
)


TEST_SOURCE = 'tests/macos/StartupIntegrationChecks.swift'
MODEL_SOURCE = 'macos/MacEveryApp.swift'
RESULT_FIELDS = {'path', 'basename', 'kind', 'size', 'mtime', 'score'}
CASE_NAMES = [
    'cold_immediate_code', 'clear_inflight', 'rapid_latest_only', 'warm_code',
    'transport_timeout_cli_success', 'transport_timeout_cli_cancel', 'final_clear',
]


WRAPPER = r'''#!/usr/bin/env python3
import ctypes,hashlib,json,os,resource,signal,subprocess,sys,time
from pathlib import Path

root=Path(__file__).resolve().parent
config=json.loads((root/'wrapper.local.json').read_text())
cancelled=False
child=None
reaped=False
term_at=None
started=time.monotonic()
class ProcUsage(ctypes.Structure):
    _fields_=[('uuid',ctypes.c_uint8*16)]+[(name,ctypes.c_uint64) for name in [
        'user_time','system_time','pkg_idle_wkups','interrupt_wkups','pageins',
        'wired_size','resident_size','phys_footprint','proc_start_abstime','proc_exit_abstime']]
libproc=ctypes.CDLL('/usr/lib/libproc.dylib')
libproc.proc_pid_rusage.argtypes=[ctypes.c_int,ctypes.c_int,ctypes.c_void_p]
libproc.proc_pid_rusage.restype=ctypes.c_int
system=ctypes.CDLL('/usr/lib/libSystem.B.dylib')
system.mach_absolute_time.restype=ctypes.c_uint64

def proc_start(pid):
    info=ProcUsage()
    return info.proc_start_abstime if libproc.proc_pid_rusage(pid,0,ctypes.byref(info))==0 else None

def event(kind,**extra):
    payload=dict(kind=kind,wrapper_pid=os.getpid(),pgid=os.getpgrp(),
                 uptime=time.monotonic(),epoch_seconds=time.time(),
                 mach_abstime=system.mach_absolute_time(),**extra)
    line=(json.dumps(payload,sort_keys=True)+'\n').encode()
    fd=os.open(root/'cli.events.jsonl',os.O_CREAT|os.O_WRONLY|os.O_APPEND,0o600)
    try: os.write(fd,line)
    finally: os.close(fd)

def on_cancel(_signum,_frame):
    global cancelled
    cancelled=True

def control():
    try: return json.loads((root/'control.json').read_text())
    except (OSError,ValueError): return {}

def signal_child(sig):
    if child is not None and not reaped:
        try: os.kill(child.pid,sig)
        except ProcessLookupError: pass

def reap(options):
    global reaped
    pid,status,usage=os.wait4(child.pid,options)
    if not pid: return None
    reaped=True
    child.returncode=os.waitstatus_to_exitcode(status)
    event('child_reaped',child_pid=pid,role=role,phase=phase,
          query_sha256=query_sha,returncode=child.returncode,cancelled=cancelled,
          user_cpu_seconds=usage.ru_utime,system_cpu_seconds=usage.ru_stime,
          lifetime_peak_rss_bytes=usage.ru_maxrss)
    return child.returncode

try:
    # All descendants remain in this wrapper-owned group. Even if the Swift
    # parent kills the wrapper, the controller has the group receipt to clean.
    if os.getpgrp()!=os.getpid(): os.setsid()
    signal.signal(signal.SIGTERM,on_cancel)
    signal.signal(signal.SIGINT,on_cancel)
    args=sys.argv[1:]
    role=args[0] if args else ''
    if role not in ['bootstrap','status','search']: raise RuntimeError('Forbidden CLI role')
    query=args[1] if role=='search' and len(args)>1 else ''
    query_sha=hashlib.sha256(query.encode()).hexdigest()
    state=control()
    phase=state.get('phase','unknown')
    event('wrapper_start',role=role,phase=phase,query_sha256=query_sha,
          proc_start_abstime=proc_start(os.getpid()))
    if cancelled: sys.exit(143)
    env={**os.environ,'MACEVERY_DB':config['db']}
    env.pop('MACEVERY_AUTH_TOKEN',None)
    # Block cancellation across spawn/receipt; reset the child's inherited mask.
    oldmask=signal.pthread_sigmask(signal.SIG_BLOCK,{signal.SIGTERM,signal.SIGINT})
    try:
        if cancelled: sys.exit(143)
        child=subprocess.Popen([config['binary'],*args],env=env,
            preexec_fn=lambda: signal.pthread_sigmask(signal.SIG_SETMASK,oldmask))
        event('child_start',child_pid=child.pid,role=role,phase=phase,query_sha256=query_sha,
              child_proc_start_abstime=proc_start(child.pid))
        if role=='search' and state.get('hold_cli'):
            signal_child(signal.SIGSTOP)
            event('child_held',child_pid=child.pid,role=role,phase=phase)
    finally: signal.pthread_sigmask(signal.SIG_SETMASK,oldmask)
    while True:
        done=reap(os.WNOHANG)
        if done is not None: break
        now=time.monotonic()
        if now-started>120: cancelled=True
        if cancelled:
            if term_at is None:
                term_at=now
                signal_child(signal.SIGCONT)
                signal_child(signal.SIGTERM)
            elif now-term_at>.075: signal_child(signal.SIGKILL)
        time.sleep(.005)
    sys.exit(143 if cancelled else done)
except BaseException as error:
    if not isinstance(error,SystemExit):
        event('wrapper_failure',error_type=type(error).__name__)
    raise
finally:
    if child is not None and not reaped:
        signal_child(signal.SIGCONT)
        signal_child(signal.SIGKILL)
        reap(0)
    usage=resource.getrusage(resource.RUSAGE_SELF)
    event('wrapper_exit',cancelled=cancelled,user_cpu_seconds=usage.ru_utime,
          system_cpu_seconds=usage.ru_stime,lifetime_peak_rss_bytes=usage.ru_maxrss)
'''


def events(path):
    values = []
    if path.exists():
        for line in path.read_text().splitlines():
            try:
                values.append(json.loads(line))
            except ValueError:
                pass  # A concurrent append may not yet be complete.
    return values


def group_exists(group):
    try:
        os.killpg(group, 0)
        return True
    except ProcessLookupError:
        return False


def process_start(pid):
    info = RusageInfoV0()
    library = ctypes.CDLL('/usr/lib/libproc.dylib')
    library.proc_pid_rusage.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
    library.proc_pid_rusage.restype = ctypes.c_int
    return info.proc_start_abstime if library.proc_pid_rusage(pid, 0, ctypes.byref(info)) == 0 else None


def cleanup_wrappers(directory):
    receipts = []
    ledger = events(directory / 'cli.events.jsonl')
    groups = {v['pgid'] for v in ledger if v['kind'] == 'wrapper_start'}
    for group in sorted(groups):
        require(group > 1 and group != os.getpgrp(), 'Unsafe wrapper cleanup group')
        identities = [(v['wrapper_pid'], v.get('proc_start_abstime')) for v in ledger
                      if v['kind'] == 'wrapper_start' and v['pgid'] == group]
        identities += [(v['child_pid'], v.get('child_proc_start_abstime')) for v in ledger
                       if v['kind'] == 'child_start' and v['pgid'] == group]

        def still_owned():
            for pid, expected in identities:
                if expected is None or process_start(pid) != expected:
                    continue
                try:
                    if os.getpgid(pid) == group:
                        return True
                except ProcessLookupError:
                    pass
            return False

        method = 'already_gone'
        if group_exists(group):
            method = 'SIGCONT_SIGTERM_identity_checked'
            for action in [signal.SIGCONT, signal.SIGTERM]:
                if not group_exists(group):
                    break
                if not still_owned():
                    method = 'identity_mismatch_no_signal'
                    break
                try:
                    os.killpg(group, action)
                except ProcessLookupError:
                    pass
            end = time.monotonic() + .3
            while group_exists(group) and time.monotonic() < end:
                time.sleep(.01)
            if group_exists(group):
                if still_owned():
                    method = 'SIGKILL_identity_checked'
                    try:
                        os.killpg(group, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                else:
                    method = 'identity_mismatch_no_signal'
                end = time.monotonic() + 2
                while group_exists(group) and time.monotonic() < end:
                    time.sleep(.01)
        receipts.append({'pgid': group, 'method': method, 'group_gone': not group_exists(group)})
    return receipts


def full_commit_sha(value):
    if re.fullmatch(r'[0-9a-fA-F]{40}', value) is None:
        raise argparse.ArgumentTypeError('--expected-source requires the full 40-digit candidate commit SHA')
    return value.lower()


def compile_harness(repo, directory, source_commit):
    model = repo / MODEL_SOURCE
    pinned = subprocess.check_output(['git', 'show', source_commit + ':' + MODEL_SOURCE], cwd=repo)
    require(model.read_bytes() == pinned, 'Production Swift differs from the expected candidate source')
    destination = directory / 'checks'
    command = ['swiftc', '-O', '-parse-as-library', '-D', 'MACEVERY_TESTING',
               '-module-cache-path', str(directory / 'module-cache'), '-target',
               os.uname().machine + '-apple-macos13.0', '-o', str(destination),
               str(model), str(repo / TEST_SOURCE)]
    with (directory / 'compile.log').open('w') as output:
        result = subprocess.run(command, stdout=output, stderr=subprocess.STDOUT)
    require(result.returncode == 0, 'Swift integration compilation failed; inspect local compile.log')
    write_json(directory / 'compile-receipt.json', {
        'status': 'PASS', 'model_source_commit': source_commit, 'model_source_sha256': sha256(model),
        'harness_source_sha256': sha256(repo / TEST_SOURCE), 'harness_sha256': sha256(destination),
    })
    return destination


def run_owned(command, environment, directory, timeout):
    directory.mkdir()
    owner = OwnedService(command, environment, directory)
    cleanup = None
    try:
        end = time.monotonic() + timeout
        while owner.reap() is None and time.monotonic() < end:
            time.sleep(.02)
        require(owner.reaped is not None, 'Owned process timed out')
        require(owner.reaped['returncode'] == 0, 'Owned process failed; inspect local stderr')
    finally:
        cleanup = owner.close(0)
        write_json(directory / 'cleanup.json', cleanup)
    require(cleanup['process_group_gone'], 'Owned process group remained')
    return cleanup


def validate_rows(rows):
    require(isinstance(rows, list) and all(isinstance(row, dict) and set(row) == RESULT_FIELDS for row in rows),
            'Search results must contain exactly the complete six-field JSON schema')


def memory_oracle(binary, environment, directory, timeout):
    """Fully close the preflight oracle service before cold-case service starts."""
    directory.mkdir()
    port = free_port()
    while port == 17649:
        port = free_port()
    token = secrets.token_hex(24)
    owner = OwnedService([str(binary), 'serve', '--addr', f'127.0.0.1:{port}', '--backend', 'memory'],
                         {**environment, 'MACEVERY_AUTH_TOKEN': token, 'MACEVERY_EXIT_WITH_PARENT': '1'}, directory)
    raw = {'status_attempts': []}
    try:
        end = time.monotonic() + timeout
        while True:
            remaining = end - time.monotonic()
            require(remaining > 0 and owner.reap() is None, 'Owned memory oracle failed to become strictly ready')
            result = request(port, '/status', token, remaining)
            raw['status_attempts'].append(result)
            if result.get('status') == 200:
                status = response_json(result)
                if status.get('ready') is True and status.get('stale') is False:
                    require(status['backend'] == 'memory', 'HTTP oracle did not use Fastest memory backend')
                    break
            time.sleep(.02)
        raw['search'] = request(port, '/search?q=code&limit=300', token, timeout)
        rows = response_json(raw['search'])
        validate_rows(rows)
        require(len(rows) == 300, 'Memory HTTP oracle did not produce 300 rows')
    finally:
        raw['cleanup'] = owner.close(port)
        write_json(directory / 'raw.local.json', raw)
    require(raw['cleanup']['process_group_gone'] and raw['cleanup']['port_closed'], 'Memory oracle service remained alive')
    return rows, raw['cleanup'], {key: status[key] for key in ['backend', 'records', 'content_revision']}


def control_service(owner, directory, stop):
    paused = False
    while not stop.is_set():
        try:
            control = json.loads((directory / 'control.json').read_text())
            requested = control.get('pause_service', False)
            if requested != paused:
                # owner is never reaped by another thread until this monitor is
                # joined, so its PID cannot be reused during this signal path.
                os.kill(owner.process.pid, signal.SIGSTOP if requested else signal.SIGCONT)
                paused = requested
                receipt = {'kind': 'service_paused' if paused else 'service_resumed',
                           'phase': control.get('phase'), 'pid': owner.process.pid,
                           'epoch_seconds': time.time(), 'uptime': time.monotonic()}
                with (directory / 'control.events.jsonl').open('a') as stream:
                    stream.write(json.dumps(receipt, sort_keys=True) + '\n')
        except (OSError, ValueError):
            pass
        stop.wait(.01)
    try:
        os.kill(owner.process.pid, signal.SIGCONT)
    except ProcessLookupError:
        pass


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--expected-source', required=True, type=full_commit_sha,
                        help='Full candidate commit SHA; production Swift and the compile receipt must match')
    parser.add_argument('--binary', type=Path, help='Frozen /tmp candidate CLI binary')
    parser.add_argument('--snapshot', type=Path, help='Frozen /tmp SQLite snapshot; never writable')
    parser.add_argument('--output', type=Path, required=True, help='New /tmp output directory')
    parser.add_argument('--harness', type=Path, help='Previously compiled checks binary')
    parser.add_argument('--compile-only', action='store_true')
    parser.add_argument('--allow-already-ready', action='store_true', help='Small-fixture smoke only; do not require proof that first query preceded load completion')
    parser.add_argument('--timeout', type=float, default=180)
    args = parser.parse_args()
    require(sys.platform == 'darwin', 'macOS Swift integration only')
    require(args.timeout >= 60, 'Timeout must accommodate two real 15s transport waits')
    require(not (args.compile_only and args.harness), '--compile-only cannot attest an existing harness')
    repo = Path(__file__).resolve().parent.parent
    source_commit = args.expected_source
    require(subprocess.check_output(['git', 'cat-file', '-t', source_commit], cwd=repo, stderr=subprocess.PIPE)
            .decode().strip() == 'commit', '--expected-source must identify an existing commit object')
    output = require_tmp(args.output)
    require(not output.exists(), 'Refusing to overwrite integration evidence')
    output.mkdir(parents=True)
    os.chmod(output, 0o700)
    harness = args.harness.resolve() if args.harness else compile_harness(repo, output, source_commit)
    require(harness.is_file() and os.access(harness, os.X_OK), 'Missing executable Swift harness')
    model_sha = sha256(repo / MODEL_SOURCE)
    pinned = subprocess.check_output(['git', 'show', source_commit + ':' + MODEL_SOURCE], cwd=repo)
    require(hashlib.sha256(pinned).hexdigest() == model_sha, 'Production Swift differs from expected candidate source')
    receipt = json.loads((harness.parent / 'compile-receipt.json').read_text())
    require(receipt['model_source_commit'] == source_commit and receipt['model_source_sha256'] == model_sha
            and receipt['harness_source_sha256'] == sha256(repo / TEST_SOURCE)
            and receipt['harness_sha256'] == sha256(harness), 'Harness compile receipt does not match current frozen inputs')
    if args.compile_only:
        print('PASS: isolated Swift integration harness compiled; no DB or service opened')
        return 0
    require(args.binary is not None and args.snapshot is not None, '--binary and --snapshot are required for execution')
    binary, snapshot = require_tmp(args.binary.resolve(strict=True)), require_tmp(args.snapshot.resolve(strict=True))
    require(binary.is_file() and os.access(binary, os.X_OK), 'Frozen CLI is not executable')
    frozen = db_identity(snapshot)
    binary_sha = sha256(binary)
    summary = {'status': 'FAIL', 'scope': 'production Swift model + isolated real CLI/HTTP; no native App/window/TCC/watch acceptance',
               'model_source_commit': source_commit, 'model_source_sha256': model_sha,
               'harness_source_sha256': sha256(repo / TEST_SOURCE), 'runner_sha256': sha256(Path(__file__)),
               'binary_sha256': binary_sha, 'harness_sha256': sha256(harness), 'cases': [], 'cleanup': {}}
    service = child = monitor = None
    monitor_stop = threading.Event()
    try:
        database = output / 'index.sqlite'
        copy = clone_or_backup(snapshot, database, args.timeout)
        write_json(output / 'copy.local.json', copy)
        environment = {**os.environ, 'MACEVERY_DB': str(database)}
        environment.pop('MACEVERY_AUTH_TOKEN', None)
        oracle_cleanup = run_owned([str(binary), 'search', 'code', '--limit', '300', '--json'], environment, output / 'oracle', args.timeout)
        rows = json.loads((output / 'oracle/service.stdout.txt').read_text())
        validate_rows(rows)
        require(isinstance(rows, list) and len(rows) == 300, 'Snapshot must have exactly 300 code results at limit 300')
        write_json(output / 'oracle-cli-code.raw.json', rows)
        nohit = '__macevery_startup_nohit_' + secrets.token_hex(24) + '__'
        nohit_cleanup = run_owned([str(binary), 'search', nohit, '--limit', '300', '--json'], environment, output / 'oracle-nohit', args.timeout)
        require(json.loads((output / 'oracle-nohit/service.stdout.txt').read_text()) == [], 'Unique no-hit oracle was not empty')
        http_rows, http_cleanup, http_status = memory_oracle(binary, environment, output / 'oracle-http', args.timeout)
        write_json(output / 'oracle-http-code.raw.json', http_rows)
        cli_by_path, http_by_path = ({row['path']: row for row in values} for values in [rows, http_rows])
        common = cli_by_path.keys() & http_by_path.keys()
        summary['oracle'] = {
            'fields': sorted(RESULT_FIELDS),
            'cli_sqlite': {'count': len(rows), 'ordered_full_json_sha256': json_digest(rows), 'cleanup': oracle_cleanup,
                           'score_min': min(v['score'] for v in rows), 'score_max': max(v['score'] for v in rows)},
            'http_memory': {'count': len(http_rows), 'ordered_full_json_sha256': json_digest(http_rows), 'cleanup': http_cleanup,
                            'score_min': min(v['score'] for v in http_rows), 'score_max': max(v['score'] for v in http_rows),
                            'strict_ready_status': http_status},
            'intersection': {'common_paths': len(common), 'common_records_equal': all(cli_by_path[p] == http_by_path[p] for p in common),
                             'ordered_results_equal': rows == http_rows},
            'nohit_count': 0, 'nohit_cleanup': nohit_cleanup,
            'cross_backend_exact_required': True,
            'boundary': 'CLI and Fastest memory HTTP must return identical complete six-field results in identical order for the same frozen binary/DB/query/limit. Candidate truncation is not an allowed exception. Oracle service is closed before cold service starts; preflight may warm OS caches, so this is functional integration rather than cold-disk performance.',
        }
        summary['exact_topk_gate_passed'] = rows == http_rows
        require(summary['exact_topk_gate_passed'], 'Exact TopK gate failed: CLI and Fastest HTTP differ in six-field results or order')
        wrapper = output / 'owned-cli-wrapper.py'
        wrapper.write_text(WRAPPER.replace('#!/usr/bin/env python3', '#!' + sys.executable, 1))
        wrapper.chmod(0o700)
        write_json(output / 'wrapper.local.json', {'binary': str(binary), 'db': str(database)})
        write_json(output / 'control.json', {'pause_service': False, 'hold_cli': False, 'phase': 'initial'})
        port = free_port()
        while port == 17649:
            port = free_port()
        token = secrets.token_hex(24) # Fixed shared test token for this owned service/proxy pair.
        config = {'root': str(output), 'wrapper': str(wrapper), 'port': port, 'token': token,
                  'query': 'code', 'nohit': nohit, 'expected_count': 300, 'timeout': args.timeout}
        write_json(output / 'config.local.json', config)
        (output / 'config.local.json').chmod(0o600)
        serve_directory = output / 'serve'
        serve_directory.mkdir()
        service = OwnedService([str(binary), 'serve', '--addr', f'127.0.0.1:{port}', '--backend', 'memory'],
            {**environment, 'MACEVERY_AUTH_TOKEN': token, 'MACEVERY_EXIT_WITH_PARENT': '1'}, serve_directory)
        # Listener announcement precedes ServiceState::load. Deliberately do not
        # wait for /status: the first real model query must race this cold load.
        deadline = time.monotonic() + 10
        while 'search service listening' not in (serve_directory / 'service.stderr.txt').read_text():
            require(time.monotonic() < deadline, 'Owned service did not announce listener')
            require(service.reap() is None, 'Owned service exited before its listener announcement')
            time.sleep(.002)
        monitor = threading.Thread(target=control_service, args=(service, output, monitor_stop), daemon=True)
        monitor.start()
        child_directory = output / 'harness'
        child_directory.mkdir()
        child = OwnedService([str(harness), '--config', str(output / 'config.local.json')], environment, child_directory)
        deadline = time.monotonic() + args.timeout
        while child.reap() is None and time.monotonic() < deadline:
            time.sleep(.02)
        require(child.reaped is not None, 'Swift harness timed out')
        require(child.reaped['returncode'] == 0, 'Swift integration failed; inspect local harness stderr and partial model-summary.json')
        cases = json.loads((output / 'model-summary.json').read_text())['cases']
        require([v['name'] for v in cases] == CASE_NAMES, 'Missing integration case')
        summary['cases'] = cases
        transport = events(output / 'transport.events.jsonl')
        starts = {v['id']: v for v in transport if v['kind'] == 'forward_start'}
        code_sha = hashlib.sha256(b'code').hexdigest()
        comparisons = []
        loaded_at = None
        for entry in transport:
            if entry['kind'] != 'underlying_complete' or entry.get('status') != 200 or 'url_error_code' in entry:
                continue
            response = json.loads((output / f"http-{entry['id']:04d}.raw.json").read_text())
            start = starts[entry['id']]
            if start['path'] == '/status' and response.get('ready') is True and response.get('stale') is False:
                loaded_at = response['loaded_at'] if loaded_at is None else min(loaded_at, response['loaded_at'])
            if start['path'] == '/search' and start['query_sha256'] == code_sha:
                validate_rows(response)
                require(response == http_rows, 'Real HTTP ordered six-field code response differs from frozen memory HTTP oracle')
                comparisons.append({'id': entry['id'], 'count': len(response), 'ordered_full_json_sha256': json_digest(response)})
        require(comparisons, 'No real code HTTP response was compared')
        first = min(v['epoch_seconds'] for v in starts.values() if v['path'] == '/search')
        loading_verified = loaded_at is not None and first < loaded_at
        summary['cold_loading'] = {'first_forward_precedes_loaded_at_whole_second': loading_verified,
                                  'allow_already_ready': args.allow_already_ready,
                                  'boundary': 'Positive proof uses conservative loaded_at whole seconds; a fast small fixture may finish before the harness starts.'}
        require(loading_verified or args.allow_already_ready, 'Could not prove first query preceded load completion; retain as failure, or use --allow-already-ready only for small-fixture smoke')
        summary['http_full_field_comparisons'] = comparisons
        for phase in ['transport_timeout_cli_success', 'transport_timeout_cli_cancel']:
            errors = [v for v in transport if v['kind'] == 'underlying_complete' and v['phase'] == phase
                      and v.get('url_error_code') == -1001]
            require(len(errors) == 1 and starts[errors[0]['id']]['request_timeout_seconds'] == 15,
                    'Missing unique real URLSession timedOut receipt at the production 15s limit')
        cli = events(output / 'cli.events.jsonl')
        begun = {v['child_pid']: v for v in cli if v['kind'] == 'child_start'}
        reaped = {v['child_pid']: v for v in cli if v['kind'] == 'child_reaped'}
        require(set(begun) == set(reaped), 'A wrapper did not retain an owned child wait4 receipt')
        intervals = sorted([(v['mach_abstime'], reaped[pid]['mach_abstime']) for pid, v in begun.items()
                            if v['role'] == 'search'])
        require(all(end <= next_start for (_start, end), (next_start, _next_end) in zip(intervals, intervals[1:])),
                'Observed real CLI search children overlapped')
        require(all(v['role'] in ['bootstrap', 'status', 'search'] for v in begun.values()), 'Unexpected CLI role')
        cold_end = min(v['epoch_seconds'] for v in starts.values() if v['phase'] == 'clear_inflight')
        summary['cold_route'] = {
            'cli_fallback_child_pids': [pid for pid, value in begun.items()
                if value['role'] == 'search' and first <= value['epoch_seconds'] < cold_end],
            'http_statuses': [value.get('status', 0) for value in transport
                if value['kind'] == 'underlying_complete' and value['phase'] == 'cold_immediate_code'
                and starts[value['id']]['path'] == '/search'],
            'boundary': 'A cold query can legitimately exercise the unchanged CLI fallback; its CPU is retained separately, not hidden as service-only startup.',
        }
        summary['cli_children'] = [{key: v[key] for key in ['child_pid', 'role', 'phase', 'returncode', 'cancelled',
            'user_cpu_seconds', 'system_cpu_seconds', 'lifetime_peak_rss_bytes']} for v in reaped.values()]
        summary['wrapper_cpu'] = [{key: v[key] for key in ['wrapper_pid', 'user_cpu_seconds', 'system_cpu_seconds', 'lifetime_peak_rss_bytes']}
                                  for v in cli if v['kind'] == 'wrapper_exit']
        require(db_identity(snapshot) == frozen and sha256(binary) == binary_sha and sha256(repo / MODEL_SOURCE) == model_sha,
                'Frozen input or production source changed')
        summary['frozen_inputs_unchanged'] = True
        summary['status'] = 'PASS'
    except BaseException as error:
        summary['failure_type'] = type(error).__name__
        partial = output / 'model-summary.json'
        if partial.exists():
            try:
                summary['cases'] = json.loads(partial.read_text())['cases']
            except (OSError, ValueError, KeyError):
                pass
        (output / 'failure.local.txt').write_text(repr(error) + '\n')
    finally:
        monitor_stop.set()
        if monitor is not None:
            monitor.join(timeout=2)
        if child is not None:
            try:
                summary['cleanup']['harness'] = child.close(0)
            except BaseException as error:
                summary['cleanup']['harness_error_type'] = type(error).__name__
        try:
            summary['cleanup']['wrappers'] = cleanup_wrappers(output)
        except BaseException as error:
            summary['cleanup']['wrapper_error_type'] = type(error).__name__
        if service is not None:
            try:
                os.kill(service.process.pid, signal.SIGCONT)
            except ProcessLookupError:
                pass
            try:
                summary['cleanup']['service'] = service.close(port)
            except BaseException as error:
                summary['cleanup']['service_error_type'] = type(error).__name__
        clean = summary['cleanup']
        oracle = summary.get('oracle', {})
        if oracle:
            clean['oracles'] = {'cli': oracle['cli_sqlite']['cleanup'], 'http': oracle['http_memory']['cleanup'],
                                'nohit': oracle['nohit_cleanup']}
        cleanup_ok = ((child is None or ('harness' in clean and clean['harness']['process_group_gone']))
                      and (service is None or ('service' in clean and clean['service']['process_group_gone'] and clean['service']['port_closed']))
                      and 'oracles' in clean and all(v['process_group_gone'] and v['port_closed'] for v in clean['oracles'].values())
                      and 'wrappers' in clean and all(v['group_gone'] for v in clean['wrappers'])
                      and not any(key.endswith('_error_type') for key in clean))
        summary['cleanup_complete'] = cleanup_ok
        if not cleanup_ok:
            summary['status'] = 'FAIL'
        write_json(output / 'summary.json', summary)
    print(json.dumps({'status': summary['status'], 'cases': len(summary['cases']), 'cleanup_complete': summary['cleanup_complete']}))
    return 0 if summary['status'] == 'PASS' else 1


if __name__ == '__main__':
    raise SystemExit(main())
