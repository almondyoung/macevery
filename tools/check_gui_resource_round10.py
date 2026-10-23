#!/usr/bin/env python3
"""Both-source production-formatter adapter for the owned round8 GUI A/B/A.

4dc already has shared formatters; the historical per-call-date baseline adapter
is inappropriate. Compile the unchanged workload with both exact production
sources and otherwise reuse owned lifecycle, sampling, result and cleanup gates.
Normal300-result model/display workload, not a UserDropped reproduction or an
allocation audit of the historical installed GUI. Cancellation gates are covered
by GUIRegressionChecks and real transport integration separately.
"""
from pathlib import Path
import platform
import shutil
import subprocess

import check_gui_resource_round8 as base


def prepare(args, output):
    repo = Path(__file__).resolve().parents[1]
    harness = repo / 'tests/macos/GUIResourcePerfChecks.swift'
    sources = {name: getattr(args, name + '_source').resolve(strict=True)
               for name in ['baseline', 'candidate']}
    hashes = {name: base.sha256(path) for name, path in sources.items()}
    root = output / 'source'
    root.mkdir()
    copies = {}
    for name, path in sources.items():
        expected = getattr(args, name + '_sha256')
        base.require(expected is None or expected == hashes[name], name + ' source SHA mismatch')
        base.require('MacEveryDisplayFormatters.shared.date' in path.read_text(), 'Production shared formatter missing')
        folder = root / name
        folder.mkdir()
        copies[name] = folder / path.name
        shutil.copyfile(path, copies[name])
        base.require(base.sha256(copies[name]) == hashes[name], 'Source changed while freezing')
    frozen_harness = root / harness.name
    shutil.copyfile(harness, frozen_harness)
    swiftc = shutil.which('swiftc')
    base.require(swiftc is not None, 'swiftc unavailable')
    target = platform.machine() + '-apple-macos13.0'
    binaries, commands = {}, {}
    for name in sources:
        binary = output / ('gui-resource-' + name)
        command = [swiftc, '-O', '-parse-as-library', '-D', 'MACEVERY_TESTING',
                   '-module-cache-path', str(output / 'module-cache'), '-target', target,
                   '-o', str(binary), str(copies[name]), str(frozen_harness)]
        with (output / ('compile-' + name + '.log')).open('x') as log:
            subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=True, timeout=240)
        binaries[name], commands[name] = binary, command
    manifest = {'source_sha256': hashes, 'source_paths_local_only': {k: str(v) for k,v in sources.items()},
                'harness_sha256': base.sha256(frozen_harness), 'runner_sha256': base.sha256(Path(__file__)),
                'base_runner_sha256': base.sha256(Path(base.__file__)), 'target': target,
                'compiler_commands': commands, 'binary_sha256': {k: base.sha256(v) for k,v in binaries.items()},
                'formatter_contract': 'Both variants compile production shared formatter; no legacy adapter',
                'workload': {key: getattr(args, key) for key in ['formatter_rounds','model_rounds','render_rounds',
                    'render','settle_seconds','locale','time_zone','blocks','sample_interval']}}
    base.write_json(output / 'manifest.local.json', manifest)
    return sources, hashes, binaries, manifest


original_write_json = base.write_json


def write_json(path, value):
    if path.name == 'summary.json' and 'limitations' in value:
        value['limitations'] = [
            'Both variants use their production shared formatter; no historical private-date adapter.'
            if item.startswith('Baseline private date body') else item for item in value['limitations']]
    return original_write_json(path, value)


if __name__ == '__main__':
    base.prepare = prepare
    base.write_json = write_json
    raise SystemExit(base.main())
