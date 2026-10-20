#!/usr/bin/env python3
"""Compare frozen GUI sources using one isolated 300-result Swift harness.

Root schedules compilation and sequential A/B/A runs separately from other CPU
work. No installed App, user DB, socket, watcher or production CLI is used.

  python3 tools/check_gui_resource_round8.py \
    --baseline-source .build/watch-recovery-20261020/baseline/source/macos/MacEveryApp.swift \
    --candidate-source macos/MacEveryApp.swift --output /tmp/macevery-gui-round8-aba

Use --compile-only to prepare without measurement. --render none omits the
optional actual ResultRow/NSHostingView offscreen phase; it is recorded, never
silently substituted after a rendering failure. The formatter-only baseline
adapter contains the exact original private function body, with its SHA saved.
This is a mechanism/model/offscreen experiment, not whole-App attribution of
the historical ~14 MiB steps or evidence that overall installed memory fell.
"""
import argparse
import ctypes
import json
import os
from pathlib import Path
import platform
import selectors
import shutil
import signal
import statistics
import subprocess
import sys
import time

from check_service_round2 import (ProcessMetrics, RusageInfoV0, require,
                                  require_tmp, sha256, write_json)


def function_body(source, signature):
    require(source.count(signature) == 1, 'Expected exactly one frozen private date function')
    opening = source.index(signature) + len(signature) - 1
    depth = 1
    cursor = opening + 1
    while depth and cursor < len(source):
        depth += (source[cursor] == '{') - (source[cursor] == '}')
        cursor += 1
    require(depth == 0, 'Unbalanced frozen date function')
    return source[opening + 1:cursor - 1]


def prepare(args, output):
    repo = Path(__file__).resolve().parent.parent
    harness = repo / 'tests/macos/GUIResourcePerfChecks.swift'
    sources = {name: getattr(args, name + '_source').resolve(strict=True)
               for name in ['baseline', 'candidate']}
    source_hashes = {name: sha256(path) for name, path in sources.items()}
    for name in sources:
        expected = getattr(args, name + '_sha256')
        require(expected is None or expected == source_hashes[name], name + ' source SHA mismatch')
    source_dir = output / 'source'
    source_dir.mkdir()
    copies = {}
    for name, path in sources.items():
        directory = source_dir / name
        directory.mkdir()
        copies[name] = directory / 'MacEveryApp.swift'
        shutil.copyfile(path, copies[name])
        require(sha256(copies[name]) == source_hashes[name], 'Source changed while freezing its copy')
    harness_copy = source_dir / harness.name
    shutil.copyfile(harness, harness_copy)
    signature = 'private func formatDate(_ value: Int64?) -> String {'
    body = function_body(copies['baseline'].read_text(), signature)
    require('DateFormatter()' in body and '.medium' in body and '.short' in body,
            'Baseline adapter must come from the original per-call date formatter')
    adapter = source_dir / 'BaselineDisplayAdapter.swift'
    adapter.write_text('import Foundation\nenum BaselineDisplayAdapter {\n'
                       '    static func date(_ value: Int64?) -> String {' + body + '}\n}\n')
    # The body bytes, including whitespace, must survive generation unchanged.
    require(function_body(adapter.read_text(), 'static func date(_ value: Int64?) -> String {') == body,
            'Generated baseline adapter changed the original function body')
    require('MacEveryDisplayFormatters.shared.date' in copies['candidate'].read_text(),
            'Candidate shared production formatter missing')
    swiftc = shutil.which('swiftc')
    require(swiftc is not None, 'swiftc not available')
    compiler = subprocess.run([swiftc, '--version'], capture_output=True, text=True, check=True).stdout
    target = platform.machine() + '-apple-macos13.0'
    binaries = {}
    commands = {}
    for name in ['baseline', 'candidate']:
        binary = output / ('gui-resource-' + name)
        command = [swiftc, '-O', '-parse-as-library', '-D', 'MACEVERY_TESTING',
                   '-module-cache-path', str(output / 'module-cache'), '-target', target,
                   '-o', str(binary), str(copies[name]), str(harness_copy)]
        if name == 'baseline':
            command += ['-D', 'MACEVERY_RESOURCE_BASELINE', str(adapter)]
        with (output / ('compile-' + name + '.log')).open('w') as log:
            result = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT)
        require(result.returncode == 0, name + ' compile failed; original log preserved')
        binaries[name], commands[name] = binary, command
    import hashlib
    manifest = {'source_sha256': source_hashes,
                'harness_sha256': sha256(harness_copy), 'runner_sha256': sha256(Path(__file__)),
                'metrics_helper_sha256': sha256(repo / 'tools/check_service_round2.py'),
                'baseline_private_date_body_sha256': hashlib.sha256(body.encode()).hexdigest(),
                'baseline_adapter_sha256': sha256(adapter), 'compiler': compiler,
                'compiler_commands': commands, 'target': target,
                'binary_sha256': {name: sha256(path) for name, path in binaries.items()},
                'source_paths_local_only': {name: str(path) for name, path in sources.items()},
                'workload': {key: getattr(args, key) for key in [
                    'formatter_rounds', 'model_rounds', 'render_rounds', 'render',
                    'settle_seconds', 'locale', 'time_zone', 'blocks', 'sample_interval']}}
    write_json(output / 'manifest.local.json', manifest)
    return sources, source_hashes, binaries, manifest


def native_identity(metrics, pid):
    value = RusageInfoV0()
    if metrics.query(pid, 0, ctypes.byref(value)) != 0:
        return None
    return {'pid': pid, 'start_abstime': int(value.proc_start_abstime),
            'uuid': bytes(value.uuid).hex()}


class OwnedHarness:
    def __init__(self, command, environment, directory, metrics):
        self.stderr = (directory / 'stderr.txt').open('wb')
        self.process = subprocess.Popen(command, env=environment, stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=self.stderr,
                                        start_new_session=True, bufsize=0)
        self.metrics, self.reaped = metrics, None
        self.identity = native_identity(metrics, self.process.pid)

    def reap(self):
        if self.reaped is None:
            pid, status, usage = os.wait4(self.process.pid, os.WNOHANG)
            if pid:
                self.process.returncode = os.waitstatus_to_exitcode(status)
                self.reaped = {'returncode': self.process.returncode,
                               'user_cpu_seconds': usage.ru_utime,
                               'system_cpu_seconds': usage.ru_stime,
                               'cpu_seconds': usage.ru_utime + usage.ru_stime,
                               'lifetime_peak_rss_bytes': usage.ru_maxrss}
        return self.reaped

    def cleanup(self):
        actions = []
        try:
            if self.reap() is None:
                # EOF also releases a child waiting for an acknowledgement if
                # the initial libproc identity was unavailable. Never signal
                # an identity that could not be established.
                self.process.stdin.close()
                can_signal = self.identity is not None and native_identity(self.metrics, self.process.pid) == self.identity
                if can_signal:
                    try:
                        os.killpg(self.process.pid, signal.SIGTERM)
                        actions.append('SIGTERM_owned_group')
                    except ProcessLookupError:
                        actions.append('owned_group_already_exited')
                else:
                    actions.append('stdin_EOF_only_identity_unconfirmed')
                deadline = time.monotonic() + 1
                while self.reap() is None and time.monotonic() < deadline:
                    time.sleep(.01)
                if self.reaped is None and can_signal:
                    require(native_identity(self.metrics, self.process.pid) == self.identity,
                            'Owned harness identity changed before SIGKILL')
                    try:
                        os.killpg(self.process.pid, signal.SIGKILL)
                        actions.append('SIGKILL_owned_group')
                    except ProcessLookupError:
                        actions.append('owned_group_already_exited')
                    deadline = time.monotonic() + 3
                    while self.reap() is None and time.monotonic() < deadline:
                        time.sleep(.01)
            try:
                os.killpg(self.process.pid, 0)
                absent = False
            except ProcessLookupError:
                absent = True
            except PermissionError:
                absent = False
        finally:
            for stream in [self.process.stdin, self.process.stdout, self.stderr]:
                if stream:
                    stream.close()
        return {'identity': self.identity, 'owned_process_reaped': self.reaped is not None,
                'owned_group_absent': absent, 'actions': actions, 'wait4': self.reaped}


def memory_summary(values):
    result = {'samples': len(values)}
    for field in ['rss_bytes', 'physical_footprint_bytes']:
        data = [value[field] for value in values if field in value]
        if data:
            ordered = sorted(data)
            position = .95 * (len(ordered) - 1)
            lower = int(position)
            upper = min(lower + 1, len(ordered) - 1)
            p95 = ordered[lower] + (ordered[upper] - ordered[lower]) * (position - lower)
            result.update({field + '_first': data[0], field + '_last': data[-1],
                           field + '_median': statistics.median(data), field + '_p95': p95,
                           field + '_delta': data[-1] - data[0], field + '_sampled_peak': max(data)})
            points = [(row['monotonic'], row[field]) for row in values if field in row]
            if len(points) > 1 and points[-1][0] - points[0][0] >= .1:
                xs = [point[0] - points[0][0] for point in points]
                xmean, ymean = statistics.mean(xs), statistics.mean(data)
                denominator = sum((x - xmean) ** 2 for x in xs)
                result[field + '_slope_bytes_per_second'] = (
                    sum((x - xmean) * (y - ymean) for x, y in zip(xs, data)) / denominator
                    if denominator else None)
    return result


def run_one(args, binary, variant, block, ordinal, output, metrics):
    directory = output / 'raw' / f'{ordinal:02d}-{variant}'
    directory.mkdir(parents=True)
    command = [str(binary), '--formatter-rounds', str(args.formatter_rounds),
               '--model-rounds', str(args.model_rounds), '--render-rounds', str(args.render_rounds),
               '--settle-seconds', str(args.settle_seconds), '--render', args.render,
               '--time-zone', args.time_zone, '-AppleLocale', args.locale, '-AppleLanguages', '(en)']
    environment = {key: value for key, value in os.environ.items() if not key.startswith('MACEVERY_')}
    environment.update(TZ=args.time_zone, LANG='en_US.UTF-8', LC_ALL='en_US.UTF-8')
    owned = None
    selector = selectors.DefaultSelector()
    events, samples, phases = [], [], {}
    phase_name, phase_start, workload_manifest = 'startup', None, None
    result = {'variant': variant, 'block': block, 'ordinal': ordinal, 'events': events,
              'samples': samples, 'phases': phases}
    error = None
    started = time.monotonic()
    try:
        owned = OwnedHarness(command, environment, directory, metrics)
        require(owned.identity is not None, 'Cannot identify owned harness')
        selector.register(owned.process.stdout, selectors.EVENT_READ)
        buffer = b''
        done = False
        next_sample = 0
        while not done:
            now = time.monotonic()
            require(now - started < args.timeout, 'Isolated harness timed out')
            if now >= next_sample:
                sample = metrics.sample(owned.process.pid)
                require('error' not in sample, 'Owned resource sample failed')
                require(native_identity(metrics, owned.process.pid) == owned.identity,
                        'Owned resource identity changed')
                samples.append(dict(sample, phase=phase_name))
                next_sample = now + args.sample_interval
            ready = selector.select(max(0, min(args.sample_interval, next_sample - time.monotonic())))
            if not ready:
                require(owned.reap() is None, 'Harness exited before done event')
                continue
            data = os.read(owned.process.stdout.fileno(), 65536)
            require(data, 'Harness stdout closed before done event')
            buffer += data
            require(len(buffer) <= 1024 * 1024, 'Harness event exceeded bound')
            while b'\n' in buffer:
                line, buffer = buffer.split(b'\n', 1)
                event = json.loads(line)
                events.append(event)
                endpoint = metrics.sample(owned.process.pid)
                require('error' not in endpoint, 'Phase endpoint resource sample failed')
                kind = event['event']
                if kind == 'manifest':
                    require(workload_manifest is None, 'Duplicate workload manifest')
                    workload_manifest = event
                    require(event['rows'] == 300, 'Wrong fixed-result count')
                    require(event['locale_identifier'].split('@', 1)[0] == args.locale,
                            'Requested process-only locale override was not applied')
                    accepted_zones = {'UTC', 'GMT', 'Etc/UTC', 'Etc/GMT'} if args.time_zone in ['UTC', 'GMT'] else {args.time_zone}
                    require(event['time_zone_identifier'] in accepted_zones,
                            'Requested process-only time zone was not applied')
                elif kind == 'phase_begin':
                    phase_name, phase_start = event['phase'], endpoint
                    require(phase_name not in phases, 'Duplicate measurement phase')
                    samples.append(dict(endpoint, phase=phase_name))
                elif kind == 'phase_end':
                    require(phase_name == event['phase'] and phase_start is not None,
                            'Unmatched phase endpoint')
                    samples.append(dict(endpoint, phase=phase_name))
                    phase_samples = [row for row in samples if row['phase'] == phase_name]
                    phases[phase_name] = dict(event, resources=memory_summary(phase_samples),
                                             libproc_endpoint_cpu_seconds=(endpoint['cpu_ns'] - phase_start['cpu_ns']) / 1e9)
                    phase_name, phase_start = 'between_phases', None
                elif kind == 'done':
                    require(event['model_released'] and event['hosting_view_released'],
                            'Harness did not release its owned objects')
                    done = True
                else:
                    raise AssertionError('Unexpected harness event')
                owned.process.stdin.write(b'continue\n')
                owned.process.stdin.flush()
        deadline = time.monotonic() + 5
        while owned.reap() is None and time.monotonic() < deadline:
            time.sleep(.01)
        require(owned.reaped is not None and owned.reaped['returncode'] == 0,
                'Harness did not exit successfully after done acknowledgement')
        required = {'idle_before', 'formatters', 'model_replace_clear', 'idle_after_work', 'release_and_settle'}
        if args.render == 'offscreen':
            required.add('result_row_offscreen')
        require(set(phases) == required, 'Incomplete phase set')
        result.update(workload_manifest=workload_manifest,
                      lifetime=owned.reaped, wall_seconds=time.monotonic() - started,
                      sampled_process_memory=memory_summary(samples))
    except Exception as failure:
        error = type(failure).__name__ + ': ' + str(failure)
        result['error'] = error
    finally:
        selector.close()
        if owned is not None:
            try:
                result['cleanup'] = owned.cleanup()
            except Exception as failure:
                result['cleanup'] = {'error': type(failure).__name__ + ': ' + str(failure)}
        else:
            result['cleanup'] = {'not_started': True}
        write_json(directory / 'result.local.json', result)
    require(error is None, error or 'Unknown harness error')
    require(result['cleanup'].get('owned_process_reaped') and result['cleanup'].get('owned_group_absent'),
            'Owned harness cleanup failed')
    return result


def sanitized_run(run):
    return {key: run[key] for key in ['variant', 'block', 'ordinal', 'workload_manifest',
                                      'phases', 'lifetime', 'wall_seconds',
                                      'sampled_process_memory', 'cleanup'] if key in run}


def paired(runs):
    blocks = []
    for block in sorted({run['block'] for run in runs}):
        group = [run for run in runs if run['block'] == block]
        require([run['variant'] for run in group] == ['baseline', 'candidate', 'baseline'], 'Incomplete A/B/A block')
        fields = {'lifetime_cpu_seconds': lambda row: row['lifetime']['cpu_seconds'],
                  'lifetime_peak_rss_bytes': lambda row: row['lifetime']['lifetime_peak_rss_bytes']}
        for name in group[0]['phases']:
            fields[name + '_cpu_seconds'] = lambda row, phase=name: row['phases'][phase]['cpu_seconds']
            for metric in ['rss_bytes', 'physical_footprint_bytes']:
                fields[name + '_' + metric + '_last'] = (
                    lambda row, phase=name, key=metric: row['phases'][phase]['resources'][key + '_last'])
        metrics = {}
        for name, getter in fields.items():
            a1, b, a2 = (getter(row) for row in group)
            a = (a1 + a2) / 2
            metrics[name] = {'baseline_before': a1, 'candidate': b, 'baseline_after': a2,
                             'baseline_pair_mean': a,
                             'candidate_change_percent': (b / a - 1) * 100 if a else None}
        blocks.append({'block': block, 'runs': [row['ordinal'] for row in group], 'metrics': metrics})
    names = blocks[0]['metrics'] if blocks else []
    medians = {name: statistics.median([block['metrics'][name]['candidate_change_percent']
                                       for block in blocks
                                       if block['metrics'][name]['candidate_change_percent'] is not None])
               for name in names if any(block['metrics'][name]['candidate_change_percent'] is not None
                                       for block in blocks)}
    return blocks, medians


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--baseline-source', required=True, type=Path)
    parser.add_argument('--candidate-source', required=True, type=Path)
    parser.add_argument('--baseline-sha256')
    parser.add_argument('--candidate-sha256')
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--compile-only', action='store_true')
    parser.add_argument('--blocks', type=int, default=2)
    parser.add_argument('--formatter-rounds', type=int, default=20)
    parser.add_argument('--model-rounds', type=int, default=30)
    parser.add_argument('--render-rounds', type=int, default=75)
    parser.add_argument('--render', choices=['offscreen', 'none'], default='offscreen')
    parser.add_argument('--settle-seconds', type=float, default=2)
    parser.add_argument('--sample-interval', type=float, default=.02)
    parser.add_argument('--timeout', type=float, default=180)
    parser.add_argument('--locale', default='en_US')
    parser.add_argument('--time-zone', default='UTC')
    args = parser.parse_args()
    require(sys.platform == 'darwin', 'macOS only')
    require(1 <= args.blocks <= 10 and .005 <= args.sample_interval <= 1 and 10 <= args.timeout <= 3600,
            'Block/sample/timeout outside bound')
    require(all(1 <= value <= 10_000 for value in [args.formatter_rounds, args.model_rounds, args.render_rounds])
            and 0 <= args.settle_seconds <= 30, 'Workload outside bound')
    output = require_tmp(args.output)
    require(not output.exists(), 'Refusing to overwrite any evidence directory')
    output.mkdir(mode=0o700, parents=True)
    summary = {'status': 'INCOMPLETE', 'runs': [],
               'validation_scope': 'Fixture/display/lifecycle equivalence; resources reported without an automatic improvement budget',
               'limitations': [
        'Isolated formatter/model/offscreen workload; not the installed App or historical footprint root cause.',
        'Baseline private date body is a verbatim adapter only in the formatter mechanism phase; count and ResultRow are production paths.',
        'URLProtocol serves exactly the same ordered 300 six-field JSON results; it never forwards to real port 17649.',
        'CPU is the harness process only; compilation/controller CPU are excluded. Phase CPU uses getrusage; wait4 gives lifetime CPU/HWM.',
        'RSS/physical footprint are separate sampled metrics; peaks are lower bounds, and ru_maxrss is lifetime RSS, not footprint.',
        'No allocation-count or causal whole-App claim; other user applications and WindowServer remain uncontrolled.',
        'Idle-after-work has a cleared model, at most the final 12-row host page, and retained 300-row fixture/JSON; not a 300-visible-result App steady state.',
        'Offscreen rendering checks a nonempty bitmap and fixed row/frame visits, not pixel-for-pixel equality or native-window acceptance.',
        'The timeout bounds each harness run only; compiler preparation is outside measurement and has no automatic compiler deadline.',
        'Status burst and stderr storm bounds belong to the separate candidate correctness checks, not simulated baseline performance.']}
    sources = None
    source_hashes = None
    try:
        sources, source_hashes, binaries, manifest = prepare(args, output)
        summary['manifest'] = {key: value for key, value in manifest.items()
                               if key not in ['compiler_commands', 'source_paths_local_only']}
        if args.compile_only:
            summary['status'] = 'COMPILED_NOT_MEASURED'
        else:
            runs = []
            metrics = ProcessMetrics()
            for block in range(args.blocks):
                for variant in ['baseline', 'candidate', 'baseline']:
                    try:
                        run = run_one(args, binaries[variant], variant, block, len(runs) + 1, output, metrics)
                    except Exception:
                        summary['failed_run'] = {'variant': variant, 'block': block, 'ordinal': len(runs) + 1,
                                                 'receipt': f'raw/{len(runs) + 1:02d}-{variant}/result.local.json'}
                        raise
                    runs.append(run)
                    summary['runs'].append(sanitized_run(run))
                    write_json(output / 'summary.json', summary)
            first = runs[0]['workload_manifest']
            require(all(run['workload_manifest'] == first for run in runs), 'Workload/environment/result digest differs')
            displays = [run['phases']['formatters']['details'] for run in runs]
            require(all(value == displays[0] for value in displays), 'Display semantics or formatter workload differs')
            summary['paired_blocks'], summary['median_paired_change_percent'] = paired(runs)
            summary.update(status='PASS', ordered_results_equal=True, display_semantics_equal=True,
                           cleanup_all_passed=True)
    except Exception as failure:
        summary.update(status='FAIL', error=type(failure).__name__ + ': ' + str(failure))
    if sources is not None and source_hashes is not None:
        try:
            summary['source_unchanged'] = all(sha256(path) == source_hashes[name] for name, path in sources.items())
        except OSError:
            summary['source_unchanged'] = False
        if not summary['source_unchanged']:
            summary.update(status='FAIL', source_change_error='Input source changed during preparation/measurement')
    write_json(output / 'summary.json', summary)
    print(json.dumps({'status': summary['status'], 'output': str(output), 'runs': len(summary['runs'])}))
    return 0 if summary['status'] in ['PASS', 'COMPILED_NOT_MEASURED'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
