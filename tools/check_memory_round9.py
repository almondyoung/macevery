#!/usr/bin/env python3
"""Root-scheduled, bounded round-9 packed-memory mechanism experiments.

Compile only (no DB/child measurement):
  python3 tools/check_memory_round9.py --baseline-lib /tmp/base/libmacevery.rlib \
    --candidate-lib /tmp/new/libmacevery.rlib --compile-only --output /tmp/r9-build
Small fixture first; then repeat with --snapshot /tmp/retained-frozen.sqlite:
  python3 tools/check_memory_round9.py --baseline-lib /tmp/base/libmacevery.rlib \
    --candidate-lib /tmp/new/libmacevery.rlib --seed-cli /tmp/new/macevery \
    --output /tmp/r9-small --rows 6000 --blocks 1 --sample-ms 20

Each A/B/A process receives a fresh APFS clone of one prepared physical mother.
The source snapshot is opened read-only by clone/backup, never by the driver.
DB preparation/mutation, correctness oracles, hashing and diagnostics are outside
the Rust load/apply/query timers. Lifetime CPU/memory include all those costs.
There is no memory pressure, cache flush, purge or live App/watcher interaction.
Large synthetic DB text (256KiB/row) deliberately exercises reserve/fragmentation
mechanisms; it is not a representative or native-filesystem-path benchmark and
cannot establish the historical footprint step's cause. --reserve-bytes can be
raised up to 70MiB to cross arenas; --churn-bytes up to 40MiB can request fallback.
Failed runs/DBs remain. Successful private run DB clones are removed by default;
the mother, exact workload, hashes, counters and bounded raw logs remain.
"""
import argparse
import ctypes
import json
import os
from pathlib import Path
import select
import signal
import sqlite3
import subprocess
import sys
import threading
import time

from check_service_round2 import (
    ProcessMetrics, RusageInfoV0, clone_or_backup, db_identity, identity,
    nonempty_wal, require, require_tmp, sha256, write_json,
)

ROOT = '/__MacEvery_Memory_Round9__'
PROBE_EXT = 'round9probe'
MAX_RAW_BYTES = 128 * 1024 * 1024
ADAPTER = r'''
#include <CommonCrypto/CommonDigest.h>
#include <mach/mach.h>
#include <mach/task_info.h>
#include <malloc/malloc.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
int round9_sha256(const unsigned char *data, size_t len, unsigned char *out) {
    if (len > 16 * 1024 * 1024) return -1;
    return CC_SHA256(data, (CC_LONG)len, out) ? 0 : -2;
}
int round9_stats_json(unsigned char *buffer, size_t capacity) {
    malloc_statistics_t stats;
    memset(&stats, 0, sizeof(stats));
    /* Public malloc.h: NULL sums all zones, not only the default zone. */
    malloc_zone_statistics(NULL, &stats);
    task_vm_info_data_t vm;
    memset(&vm, 0, sizeof(vm));
    mach_msg_type_number_t count = TASK_VM_INFO_COUNT;
    kern_return_t kr = task_info(mach_task_self(), TASK_VM_INFO,
                                (task_info_t)&vm, &count);
    /* VM fields below are valid through REV1, not assumed on older replies. */
    int valid = kr == KERN_SUCCESS && count >= TASK_VM_INFO_REV1_COUNT;
    return snprintf((char *)buffer, capacity,
        "{\"malloc_blocks_in_use\":%u,\"malloc_size_in_use_bytes\":%zu,"
        "\"malloc_max_size_in_use_bytes\":%zu,\"malloc_size_allocated_bytes\":%zu,"
        "\"vm_status\":%d,\"vm_returned_count\":%u,\"vm_required_rev1_count\":%u,"
        "\"vm_fields_valid\":%s,\"vm_virtual_bytes\":%llu,"
        "\"vm_resident_bytes\":%llu,\"vm_internal_bytes\":%llu,"
        "\"vm_external_bytes\":%llu,\"vm_reusable_bytes\":%llu,"
        "\"vm_compressed_bytes\":%llu,\"vm_compressed_peak_bytes\":%llu,"
        "\"vm_compressed_lifetime_bytes\":%llu,\"vm_phys_footprint_bytes\":%llu}",
        stats.blocks_in_use, stats.size_in_use, stats.max_size_in_use,
        stats.size_allocated, kr, count, TASK_VM_INFO_REV1_COUNT,
        valid ? "true" : "false", (unsigned long long)(valid ? vm.virtual_size : 0),
        (unsigned long long)(valid ? vm.resident_size : 0),
        (unsigned long long)(valid ? vm.internal : 0),
        (unsigned long long)(valid ? vm.external : 0),
        (unsigned long long)(valid ? vm.reusable : 0),
        (unsigned long long)(valid ? vm.compressed : 0),
        (unsigned long long)(valid ? vm.compressed_peak : 0),
        (unsigned long long)(valid ? vm.compressed_lifetime : 0),
        (unsigned long long)(valid ? vm.phys_footprint : 0));
}
'''


class PinnedMetrics(ProcessMetrics):
    def sample(self, pid):
        sample = super().sample(pid)
        info = RusageInfoV0()
        if self.query(pid, 0, ctypes.byref(info)) == 0:
            sample.update(start_abstime=info.proc_start_abstime,
                          uuid=bytes(info.uuid).hex())
        else:
            sample['identity_error'] = True
        return sample


class BoundedSampler:
    """Streaming evidence; never retains unbounded samples in controller RAM."""
    def __init__(self, pid, interval, path, max_samples):
        self.pid, self.interval, self.path = pid, interval, path
        self.max_samples = max_samples
        self.metrics = PinnedMetrics()
        self.lock = threading.Lock()
        self.stop_event = threading.Event()
        self.phase = 'startup'
        self.count = self.bytes = 0
        self.valid_samples = self.failed_samples = 0
        self.nonpositive_footprint_samples = 0
        self.identity = None
        self.error = None
        self.phases = {}
        self.stream = path.open('x')
        self.thread = threading.Thread(target=self.loop, daemon=True)
        self.thread.start()

    def loop(self):
        try:
            while not self.stop_event.is_set():
                self.capture()
                self.stop_event.wait(self.interval)
        except BaseException as error:
            self.error = repr(error)
            self.stop_event.set()

    def capture(self):
        with self.lock:
            sample = self.metrics.sample(self.pid)
            sample['phase'] = self.phase
            encoded = json.dumps(sample, separators=(',', ':')) + '\n'
            require(self.count < self.max_samples and self.bytes + len(encoded) <= MAX_RAW_BYTES,
                    'Sampling evidence bound reached; run fails rather than silently truncating')
            self.stream.write(encoded)
            self.count += 1
            self.bytes += len(encoded)
            if 'rss_bytes' in sample and 'start_abstime' in sample:
                current = (sample['start_abstime'], sample['uuid'])
                if self.identity is None:
                    self.identity = current
                require(current == self.identity, 'Owned process identity changed')
                # proc_pid_rusage can succeed as the owned child exits while
                # returning zero footprint. Retain the raw read, but do not
                # use it as a valid memory point or a phase's last endpoint.
                if sample.get('physical_footprint_bytes', 0) <= 0:
                    self.failed_samples += 1
                    self.nonpositive_footprint_samples += 1
                    return
                self.valid_samples += 1
                phase = self.phases.setdefault(self.phase, {
                    'samples': 0, 'first': sample, 'last': sample,
                    'sampled_peak_rss_bytes': 0, 'sampled_peak_footprint_bytes': 0})
                phase['samples'] += 1
                phase['last'] = sample
                phase['sampled_peak_rss_bytes'] = max(phase['sampled_peak_rss_bytes'], sample['rss_bytes'])
                phase['sampled_peak_footprint_bytes'] = max(phase['sampled_peak_footprint_bytes'], sample['physical_footprint_bytes'])
            else:
                self.failed_samples += 1

    def set_phase(self, phase):
        with self.lock:
            self.phase = phase
        self.capture()

    def close(self):
        if self.stream.closed:
            return
        self.stop_event.set()
        self.thread.join(timeout=3)
        require(not self.thread.is_alive(), 'Sampler did not finish')
        self.stream.flush()
        self.stream.close()


def reap_owned(process, seconds=3):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        pid, status, usage = os.wait4(process.pid, os.WNOHANG)
        if pid:
            process.returncode = os.waitstatus_to_exitcode(status)
            return pid, status, usage
        time.sleep(.01)
    raise TimeoutError('Owned child did not exit after EOF')


def compile_drivers(args, output, manifest):
    adapter = output / 'sdk-adapter.c'
    adapter.write_text(ADAPTER)
    obj = output / 'sdk-adapter.o'
    command = ['xcrun', 'clang', '-std=c11', '-O2', '-Wall', '-Wextra',
               '-Werror', '-Wno-deprecated-declarations', '-c', str(adapter), '-o', str(obj)]
    with (output / 'compile-adapter.log').open('x') as log:
        subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=True, timeout=180)
    manifest['adapter'] = {'source_sha256': sha256(adapter), 'object_sha256': sha256(obj),
                           'command': command,
                           'sdk': subprocess.check_output(['xcrun', '--show-sdk-path'], text=True).strip()}
    driver = Path(__file__).with_name('memory_round9.rs').resolve()
    binaries = {}
    for variant, library in [('baseline', args.baseline_lib), ('candidate', args.candidate_lib)]:
        library = library.resolve(strict=True)
        binary = output / ('driver-' + variant)
        command = ['rustc', '--edition=2021', '-O', '-C', 'lto=fat', str(driver),
                   '--extern', 'macevery=' + str(library), '-C', 'link-arg=' + str(obj), '-o', str(binary)]
        with (output / ('compile-' + variant + '.log')).open('x') as log:
            subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=True, timeout=180)
        binaries[variant] = binary
        manifest['libraries'][variant] = {'library': identity(library, True), 'binary': identity(binary, True),
                                          'command': command}
    return binaries


def prepare_mother(args, output, manifest):
    mother = output / 'mother.sqlite'
    if args.snapshot:
        source = require_tmp(args.snapshot.resolve(strict=True))
        require(not source.is_symlink() and source.is_file() and not nonempty_wal(source),
                'Use a closed, sidecar-free frozen snapshot')
        manifest['source'] = {'identity': db_identity(source), 'sha256': sha256(source)}
        manifest['source_copy'] = clone_or_backup(source, mother, args.timeout)
    completed = subprocess.run([str(args.seed_cli.resolve(strict=True)), 'bootstrap', '--json'],
                               env={**os.environ, 'MACEVERY_DB': str(mother)},
                               capture_output=True, timeout=180)
    (output / 'preparation.stdout').write_bytes(completed.stdout)
    (output / 'preparation.stderr').write_bytes(completed.stderr)
    require(completed.returncode == 0, 'Mother schema/bootstrap failed')
    with sqlite3.connect(mother) as connection:
        existing = connection.execute('SELECT path FROM files WHERE path >= ? AND path < ? LIMIT 1',
                                      (ROOT, ROOT + '0')).fetchone()
        require(existing is None, 'Snapshot already contains round-9 fixture; refusing overwrite')
        columns = '(path,path_lower,basename,basename_lower,ext_lower,kind,size,mtime,ctime,dev,inode,indexed_at)'
        sql = 'INSERT INTO files' + columns + ' VALUES(?,?,?,?,?,?,?,?,?,?,?,?)'
        if not args.snapshot:
            def ordinary():
                for i in range(args.rows):
                    name = f'code-{i:08}.rs'
                    path = f'/round9-small/tree-{i % 100}/{name}'
                    yield (path, path, name, name, 'rs', 'file', i % 4096,
                           1700000000 + i, 1600000000 + i, 1, i + 100, 1)
            connection.executemany(sql, ordinary())
        def probes():
            for i in range(args.probe_rows):
                name = f'code-{i:08}.{PROBE_EXT}'
                path = f'{ROOT}/probes/{name}'
                yield (path, path, name, name, PROBE_EXT, 'file', i,
                       1700000000 + i, 1600000000 + i, 1, i + 200, 1)
        connection.executemany(sql, probes())
        connection.commit()
        connection.execute('PRAGMA wal_checkpoint(TRUNCATE)')
        connection.execute('PRAGMA journal_mode=DELETE')
        require(connection.execute('PRAGMA quick_check').fetchone() == ('ok',), 'Mother quick_check failed')
    require(not nonempty_wal(mother), 'Mother has nonempty WAL')
    manifest['mother'] = {'identity': db_identity(mother), 'sha256': sha256(mother)}
    return mother


def run_owned(binary, database, directory, args):
    (directory / '.macevery-memory-round9-owned').write_text('macevery-memory-round9-v1\n')
    command = [str(binary), '--owned-root', str(directory), '--owned-db', str(database),
               '--idle-ms', str(args.idle_ms), '--reserve-bytes', str(args.reserve_bytes),
               '--churn-bytes', str(args.churn_bytes), '--queries', args.queries]
    result = {'command': command, 'status': 'FAIL', 'events': []}
    stderr = (directory / 'stderr.log').open('xb')
    raw = (directory / 'events.jsonl').open('xb')
    process = None
    sampler = None
    reaped = False
    started = time.monotonic()
    try:
        process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   stderr=stderr, start_new_session=True)
        # This function is the sole wait4 owner. No poll()/wait() can release the
        # PID while the sampler is active; an unreaped child cannot be reused.
        sampler = BoundedSampler(process.pid, args.sample_ms / 1000,
                                 directory / 'samples.jsonl', args.max_samples)
        result['pid'] = process.pid
        os.set_blocking(process.stdout.fileno(), False)
        buffered = b''
        total = 0
        while True:
            remaining = args.timeout - (time.monotonic() - started)
            require(remaining > 0, 'Owned memory driver exceeded bounded total deadline')
            require(sampler.error is None, 'Sampler failed: ' + str(sampler.error))
            if not select.select([process.stdout], [], [], min(remaining, 1))[0]:
                continue
            chunk = os.read(process.stdout.fileno(), 65536)
            if not chunk:
                require(not buffered, 'Truncated final driver JSONL')
                break
            buffered += chunk
            require(len(buffered) <= 65536, 'Oversized pending driver line')
            while b'\n' in buffered:
                line, buffered = buffered.split(b'\n', 1)
                total += len(line) + 1
                require(len(line) <= 32768 and total <= 8 * 1024 * 1024
                        and len(result['events']) < 1024, 'Driver event evidence bound exceeded')
                event = json.loads(line)
                result['events'].append(event)
                raw.write(line + b'\n')
                if event['event'] == 'phase_begin':
                    sampler.set_phase(event['phase'])
                    process.stdin.write(b'go\n')
                    process.stdin.flush()
                elif event['event'] == 'phase_end':
                    sampler.capture()
                    sampler.set_phase('between_phases')
                elif event['event'] == 'verification_begin':
                    sampler.set_phase('verification:' + event['phase'])
                elif event['event'] == 'verification_end':
                    sampler.set_phase('between_phases')
        sampler.close()
        require(sampler.error is None, 'Sampler failed: ' + str(sampler.error))
        pid, status, usage = reap_owned(process)
        reaped = True
        process.returncode = os.waitstatus_to_exitcode(status)
        result.update(exit_code=process.returncode, lifetime_cpu_seconds=usage.ru_utime + usage.ru_stime,
                      lifetime_peak_rss_bytes=usage.ru_maxrss, sampled_identity=sampler.identity,
                      phases_sampled=sampler.phases, sample_count=sampler.count,
                      valid_sample_count=sampler.valid_samples,
                      failed_sample_count=sampler.failed_samples, sample_bytes=sampler.bytes)
        result['nonpositive_footprint_sample_count'] = sampler.nonpositive_footprint_samples
        require(pid == process.pid and process.returncode == 0, 'Owned memory driver failed; retain stderr and DB')
        require(result['events'] and result['events'][-1].get('event') == 'complete', 'Driver completion missing')
        require(sampler.identity is not None and sampler.valid_samples >= 2, 'No valid pinned memory samples')
        result['status'] = 'PASS'
    except BaseException as error:
        result['error'] = repr(error)
        raise
    finally:
        if sampler is not None and not sampler.stream.closed:
            try:
                sampler.close()
            except BaseException as error:
                result['sampler_cleanup_error'] = repr(error)
        if process is not None and not reaped:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            _, status, usage = reap_owned(process)
            process.returncode = os.waitstatus_to_exitcode(status)
            result.update(cleanup_exit_code=process.returncode,
                          failed_lifetime_cpu_seconds=usage.ru_utime + usage.ru_stime,
                          failed_lifetime_peak_rss_bytes=usage.ru_maxrss,
                          owned_process_reaped=True)
        if process is not None:
            process.stdin.close()
            process.stdout.close()
        raw.flush()
        raw.close()
        stderr.close()
        result['wall_seconds_entire_run'] = time.monotonic() - started
        result['events_sha256'] = sha256(directory / 'events.jsonl')
        if (directory / 'samples.jsonl').exists():
            result['samples_sha256'] = sha256(directory / 'samples.jsonl')
        write_json(directory / 'summary.local.json', result)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--baseline-lib', type=Path, required=True)
    parser.add_argument('--candidate-lib', type=Path, required=True)
    parser.add_argument('--seed-cli', type=Path)
    parser.add_argument('--snapshot', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--compile-only', action='store_true')
    parser.add_argument('--rows', type=int, default=6000)
    parser.add_argument('--probe-rows', type=int, default=1100)
    parser.add_argument('--blocks', type=int, default=1)
    parser.add_argument('--sample-ms', type=float, default=20)
    parser.add_argument('--max-samples', type=int, default=100000)
    parser.add_argument('--idle-ms', type=int, default=1000)
    parser.add_argument('--churn-bytes', type=int, default=34 * 1024 * 1024)
    parser.add_argument('--reserve-bytes', type=int, default=8 * 1024 * 1024)
    parser.add_argument('--queries', default='code,e,rs,Σ,__macevery_round9_nohit__')
    parser.add_argument('--timeout', type=float, default=1800)
    parser.add_argument('--keep-run-dbs', action='store_true')
    args = parser.parse_args()
    require(sys.platform == 'darwin', 'macOS only')
    require(args.compile_only or args.seed_cli is not None, '--seed-cli required for isolated mother preparation')
    require(1 <= args.blocks <= 3 and 1000 <= args.rows <= 100000 and 1025 <= args.probe_rows <= 2048,
            'Fixture/block bound exceeded')
    require(5 <= args.sample_ms <= 20 and 1000 <= args.max_samples <= 400000
            and 60 <= args.timeout <= 3600 and 0 <= args.idle_ms <= 10000,
            'Sampling/time bounds exceeded')
    require(0 <= args.reserve_bytes <= 70 * 1024 * 1024 and 0 <= args.churn_bytes <= 40 * 1024 * 1024,
            'Text mechanism bound exceeded')
    output = require_tmp(args.output.resolve())
    require(not output.exists(), 'Output already exists; preserve earlier evidence')
    output.mkdir(mode=0o700, parents=True)
    source = Path(__file__).resolve()
    driver = source.with_name('memory_round9.rs')
    manifest = {'schema': 1, 'arguments': {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
                'controller_sha256': sha256(source), 'driver_sha256': sha256(driver), 'libraries': {},
                'order': ['baseline', 'candidate', 'baseline'],
                'compiler': subprocess.check_output(['rustc', '--version'], text=True).strip(),
                'notes': ['Root must prove each supplied library matches its frozen source; hash identity alone is not that proof.',
                          'Fresh APFS clone of one physical mother per process, no cold-cache claim.',
                          'Rust phase CPU/wall exclude SQLite mutation, validation, hashing, diagnostics and gate wait.',
                          'Sampled memory/lifetime CPU include validation and synthetic payload allocations; peaks are lower bounds.',
                          'Packed capacity, malloc aggregate, VM categories, RSS and footprint are different measures, not an additive partition.',
                          'malloc/VM queries have observer cost outside phase timers and may allocate small diagnostic buffers.',
                          'Each query is exact full13fields+score against SQLite search. This five-query mechanism test does not replace full Top300 matrix.',
                          'Long synthetic text and requested reserve/churn bytes are mechanism inputs, not native path lengths or retained capacity.',
                          'No allocator purge/pressure, mmap advice or production app interaction.',
                          'This experiment cannot retrospectively attribute the observed historical step.']}
    report = {'status': 'FAIL', 'runs': [], 'source_unchanged': None}
    mother = None
    try:
        binaries = compile_drivers(args, output, manifest)
        write_json(output / 'manifest.local.json', manifest)
        if args.compile_only:
            report.update(status='COMPILED_ONLY', no_process_measurement_run=True)
            return 0
        manifest['seed_cli'] = identity(args.seed_cli.resolve(strict=True), True)
        mother = prepare_mother(args, output, manifest)
        write_json(output / 'manifest.local.json', manifest)
        for block in range(args.blocks):
            for ordinal, variant in enumerate(manifest['order']):
                directory = output / f'b{block}-{ordinal}-{variant}'
                directory.mkdir(mode=0o700)
                database = directory / 'index.sqlite'
                copied = clone_or_backup(mother, database, args.timeout)
                require(copied['method'].startswith('APFS_clonefile'), 'APFS clone required for same-layout comparison')
                run = run_owned(binaries[variant], database, directory, args)
                run.update(block=block, ordinal=ordinal, variant=variant, input_copy=copied)
                # Each query after each mutation is ordered/fieldwise equal to
                # SQLite; cross-version hashes also expose shared-path drift.
                run['query_hashes'] = {e['phase'] + '/' + e['query']: [e['count'], e['sha256']]
                                       for e in run['events'] if e['event'] == 'query_verified'}
                if report['runs']:
                    require(run['query_hashes'] == report['runs'][0]['query_hashes'], 'Cross-run ordered Top300 digest mismatch')
                report['runs'].append(run)
                if not args.keep_run_dbs:
                    run['removed_successful_owned_db_files'] = []
                    for suffix in ['', '-wal', '-shm']:
                        path = Path(str(database) + suffix)
                        if path.exists():
                            require(not path.is_symlink() and path.is_file(), 'Unexpected owned DB file type')
                            run['removed_successful_owned_db_files'].append(identity(path))
                            path.unlink()
                write_json(output / 'results.local.json', report)
                print(f'b{block}-{ordinal}-{variant}: PASS', flush=True)
        require(sha256(mother) == manifest['mother']['sha256'], 'Prepared mother changed')
        if args.snapshot:
            require(db_identity(args.snapshot.resolve()) == manifest['source']['identity']
                    and sha256(args.snapshot.resolve()) == manifest['source']['sha256'], 'Frozen source changed')
        report.update(status='PASS', source_unchanged=True, all_full_13_field_query_hashes_equal=True)
    except BaseException as error:
        report['error'] = repr(error)
        raise
    finally:
        write_json(output / 'manifest.local.json', manifest)
        write_json(output / 'summary.local.json', report)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
