#!/usr/bin/env python3
"""Root-scheduled audit-off/on/off idle comparison on same-layout owned clones.

One production 60s timer tick is covered by each >=65s idle interval. No search,
refresh, pressure, purge or live App action. Exact index capacity must stay fixed.
Point RSS/footprint, sampled peaks and phase CPU are distinct measurements.
"""
import argparse
import json
import os
from pathlib import Path
import secrets
import time

from check_service_round2 import (OwnedService, ProcessMetrics, clone_or_backup,
                                 free_port, identity, request, response_json,
                                 require, require_tmp, sha256, write_json)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--binary', required=True, type=Path)
    p.add_argument('--snapshot', required=True, type=Path)
    p.add_argument('--output', required=True, type=Path)
    p.add_argument('--seconds', type=int, default=65)
    args = p.parse_args()
    require(65 <= args.seconds <= 90, 'bounded interval must cover a production timer tick')
    output = require_tmp(args.output)
    output.mkdir(parents=True, exist_ok=False)
    binary = args.binary.resolve(strict=True)
    snapshot = args.snapshot.resolve(strict=True)
    report = {'status': 'FAIL', 'binary': identity(binary, True), 'source': identity(snapshot, True),
              'runs': [], 'scope': 'owned source-identical candidate audit-off/on/off; no searches'}
    metrics = ProcessMetrics()
    try:
        mother = output / 'mother.sqlite'
        clone_or_backup(snapshot, mother, 120)
        for ordinal, enabled in enumerate([False, True, False]):
            require(sha256(binary) == report['binary']['sha256'], 'tested binary changed')
            directory = output / f'run-{ordinal}'
            directory.mkdir()
            db = directory / 'index.sqlite'
            copied = clone_or_backup(mother, db, 120)
            require(copied['method'].startswith('APFS_clonefile'), 'same physical mother clone required')
            port = free_port()
            token = secrets.token_hex(24)
            env = {**os.environ, 'MACEVERY_DB': str(db), 'MACEVERY_AUTH_TOKEN': token,
                   'MACEVERY_EXIT_WITH_PARENT': '1',
                   'MACEVERY_SERVE_MEMORY_LOG': '1' if enabled else '0'}
            service = OwnedService([str(binary), 'serve', '--addr', f'127.0.0.1:{port}', '--backend', 'memory'], env, directory)
            result = {'enabled': enabled, 'ordinal': ordinal, 'input_clone': copied, 'samples': []}
            try:
                deadline = time.monotonic() + 60
                while True:
                    require(time.monotonic() < deadline and service.reap() is None, 'owned service startup failed')
                    try:
                        value = response_json(request(port, '/status', token, 1))
                        if value.get('ready') and not value.get('stale'):
                            break
                    except Exception:
                        pass
                    time.sleep(0.1)
                result['status_before'] = value
                time.sleep(0.5)  # drain readiness handlers, outside measured idle
                started = time.monotonic()
                for step in range(args.seconds + 1):
                    sample = metrics.sample(service.process.pid)
                    require('error' not in sample, 'owned service metric unavailable')
                    sample['elapsed_seconds'] = time.monotonic() - started
                    result['samples'].append(sample)
                    if step < args.seconds:
                        time.sleep(1)
                before, after = result['samples'][0], result['samples'][-1]
                result.update(wall_seconds=after['elapsed_seconds']-before['elapsed_seconds'],
                              cpu_seconds=(after['cpu_ns']-before['cpu_ns'])/1e9,
                              peak_rss_bytes=max(s['rss_bytes'] for s in result['samples']),
                              peak_footprint_bytes=max(s['physical_footprint_bytes'] for s in result['samples']),
                              end_rss_bytes=after['rss_bytes'], end_footprint_bytes=after['physical_footprint_bytes'])
                result['status_after'] = response_json(request(port, '/status', token, 5))
                require(result['cpu_seconds'] >= 0, 'CPU counter moved backwards')
                require(result['status_after']['ready'] and not result['status_after']['stale']
                        and result['status_after']['backend'] == 'memory', 'idle service identity changed')
                for key in ['actual_index_bytes', 'content_revision', 'loaded_records', 'reload_count', 'incremental_applies']:
                    require(result['status_before'][key] == result['status_after'][key], 'idle index mutation: ' + key)
                audit = directory / 'macevery-serve-memory.jsonl'
                records = []
                for path in [audit.with_suffix('.jsonl.1'), audit]:
                    if path.exists():
                        require(path.stat().st_size <= 512*1024, 'audit file bound exceeded')
                        result.setdefault('audit_files', []).append(identity(path, True))
                        records.extend(json.loads(line) for line in path.read_text().splitlines())
                require(bool(records) == enabled, 'audit enable flag did not take effect')
                if enabled:
                    require(sum(r['phase'] == 'idle' for r in records) >= 2, 'production idle timer did not tick')
                    require(all(r['vm_error'] is None and r['cpu_error'] is None for r in records), 'native metric API failed')
                    result['audit_records'] = records
                report['runs'].append(result)
            finally:
                result['exit'] = service.close(port)
                require(result['exit']['process_group_gone'] and result['exit']['port_closed'], 'owned service did not exit')
            write_json(output / 'results.local.json', report)
            print(json.dumps({k:result[k] for k in ['enabled','cpu_seconds','wall_seconds','end_rss_bytes','end_footprint_bytes']}), flush=True)
        require(len({r['status_before']['actual_index_bytes'] for r in report['runs']}) == 1, 'cross-run capacity differs')
        require(sha256(snapshot) == report['source']['sha256'], 'source changed')
        require(sha256(binary) == report['binary']['sha256'], 'tested binary changed')
        report['status'] = 'PASS'
    finally:
        write_json(output / 'summary.local.json', report)


if __name__ == '__main__':
    main()
