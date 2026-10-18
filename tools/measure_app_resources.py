#!/usr/bin/env python3
"""Read CPU/RSS of the exact local App bundle processes, without changing them."""
import argparse
import json
from pathlib import Path
import subprocess
import time

from check_service_round2 import ProcessMetrics


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--app', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--seconds', type=int, default=20)
    parser.add_argument('--note', default='Short observation; other user apps remain running.')
    args = parser.parse_args()
    if not 1 <= args.seconds <= 60 or args.output.exists():
        parser.error('seconds must be 1..60 and output must not already exist')
    app = args.app.resolve(strict=True)
    prefix = str(app / 'Contents/MacOS') + '/'
    processes = []
    for line in subprocess.check_output(['/bin/ps', '-axo', 'pid=,ppid=,comm='], text=True).splitlines():
        fields = line.split(None, 2)
        if len(fields) == 3 and fields[2].startswith(prefix):
            processes.append({'pid': int(fields[0]), 'parent': int(fields[1]), 'executable': fields[2]})
    if not processes:
        raise RuntimeError('No processes from the exact App bundle are running')
    metrics = ProcessMetrics()
    start = time.monotonic()
    samples = []
    for step in range(args.seconds + 1):
        samples.append({'elapsed_seconds': time.monotonic() - start,
                        'processes': {str(row['pid']): metrics.sample(row['pid']) for row in processes}})
        if step < args.seconds:
            time.sleep(1)
    elapsed = samples[-1]['elapsed_seconds'] - samples[0]['elapsed_seconds']
    summary = []
    for row in processes:
        key = str(row['pid'])
        before, after = samples[0]['processes'][key], samples[-1]['processes'][key]
        if 'error' in before or 'error' in after:
            raise RuntimeError(f'Process observation failed or exited: {row["pid"]}')
        cpu = (after['cpu_ns'] - before['cpu_ns']) / 1e9
        summary.append({**row, 'cpu_seconds': cpu, 'average_cpu_percent_one_core': cpu / elapsed * 100,
                        'end_rss_bytes': after['rss_bytes'],
                        'end_physical_footprint_bytes': after['physical_footprint_bytes']})
    result = {'note': args.note, 'wall_seconds': elapsed, 'processes': summary,
              'total_average_cpu_percent_one_core': sum(row['average_cpu_percent_one_core'] for row in summary),
              'total_end_rss_bytes': sum(row['end_rss_bytes'] for row in summary), 'samples': samples}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open('x') as stream:
        json.dump(result, stream, indent=2)
        stream.write('\n')
    print(json.dumps({key: value for key, value in result.items() if key != 'samples'}, indent=2))


if __name__ == '__main__':
    main()
