#!/usr/bin/env python3
"""Round-4 short-query entrypoint; reuse round-1 compilation, sampling and SHA256.

Example (root coordinates sequential performance work):
  python3 tools/compare_round4_short.py --baseline-lib /tmp/base/libmacevery.rlib \
    --candidate-lib /tmp/new/libmacevery.rlib --dataset snapshot \
    --snapshot /tmp/frozen.sqlite --rounds 2 --order aba --limit 300 \
    --sample-ms 5 --output /tmp/short-aba

Extra options: --list-queries; --query-order forward|reverse; --load-only.
--queries takes IDs from --list-queries and preserves their supplied order.
The default set has two common ASCII terms and one observed-frequency term for
each length 1..7 bytes, six Unicode terms, broad and the original long no-hit.
"rare" names do not assert zero hits. Optional NUL controls are excluded from
the default performance mix; native paths cannot contain NUL, but counts are
still recorded. Their timings must not be presented as typical user searches.

Use load-only, isolated single-query processes, reversed order and repeated
queries to investigate page-touch/allocator effects. Packed capacity, process
RSS, optional macOS physical footprint and lifetime high-water RSS are distinct
measurements. Query CPU excludes JSON/diagnostics; process RSS includes them.
Neither this wrapper nor its driver touches the passed frozen source snapshot.
"""
import argparse
import json
from pathlib import Path
import re
import sys

import compare_round1 as base


DRIVER = Path(__file__).with_name('perf_round4_short.rs')


def catalog():
    # Read the driver's literal table so printed IDs/query bytes cannot drift
    # from the executable's workload. No source is rewritten or generated.
    pattern = r'^\s*\("([^"\\]+)", "((?:[^"\\]|\\.)*)", (true|false)\),\s*$'
    cases = []
    for identifier, literal, path_only in re.findall(pattern, DRIVER.read_text(), re.M):
        value = json.loads('"' + literal.replace('\\0', '\\u0000') + '"')
        cases.append({'id': identifier, 'query': value, 'utf8_bytes': len(value.encode()),
                      'characters': len(value), 'lowercase_utf8_bytes': len(value.lower().encode()),
                      'path_only': path_only == 'true'})
    if not cases or len({case['id'] for case in cases}) != len(cases):
        raise ValueError('Driver query table is absent or has duplicate IDs')
    return cases


def take_option(arguments, name):
    """Remove an inherited value option, preserving argparse's last-value rule."""
    remaining, found = [], None
    iterator = iter(arguments)
    for value in iterator:
        if value == name:
            try:
                found = next(iterator)
            except StopIteration:
                raise ValueError(name + ' requires a value') from None
        elif value.startswith(name + '='):
            found = value[len(name) + 1:]
        else:
            remaining.append(value)
    return remaining, found


def main():
    parser = argparse.ArgumentParser(description=__doc__, add_help=False)
    parser.add_argument('--list-queries', action='store_true')
    parser.add_argument('--query-order', choices=['forward', 'reverse'], default='forward')
    parser.add_argument('--load-only', action='store_true')
    own, forwarded = parser.parse_known_args()
    cases = catalog()
    if own.list_queries:
        print(json.dumps(cases, ensure_ascii=False, indent=2))
        return 0
    try:
        forwarded, queries = take_option(forwarded, '--queries')
        if own.load_only and queries is not None:
            parser.error('--load-only cannot be combined with --queries')
        if own.load_only:
            names = []
        elif queries is None:
            names = [case['id'] for case in cases if case['id'].startswith(('common_', 'rare_', 'unicode_'))]
            names += ['broad', 'nohit']
        else:
            names = queries.split(',') if queries else []
        known = {case['id'] for case in cases}
        if any(name not in known for name in names):
            parser.error('Unknown query ID; see --list-queries')
        if len(set(names)) != len(names):
            parser.error('Duplicate query IDs reuse sampler phase names; use --query-rounds for repetition')
        if own.query_order == 'reverse':
            names.reverse()
        forwarded += ['--queries', ','.join(names)]
        # A memory driver means Vec<FileRecord>, even when the app's memory mode
        # internally uses packed storage. Enforce the intended packed A/B here.
        for option in ['--backend', '--baseline-backend', '--candidate-backend']:
            forwarded, selected = take_option(forwarded, option)
            if selected not in [None, 'compact']:
                parser.error('Round-4 short comparisons require compact storage for both versions')
        forwarded += ['--backend', 'compact']
    except ValueError as error:
        parser.error(str(error))
    sys.argv = [sys.argv[0], *forwarded]
    return base.main(driver_source=DRIVER, entrypoint_source=Path(__file__))


if __name__ == '__main__':
    raise SystemExit(main())
