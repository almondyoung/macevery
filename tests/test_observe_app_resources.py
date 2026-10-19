"""Observer tests use fake clocks/processes and only test-owned files/databases."""
import copy
import importlib.util
import json
import os
from pathlib import Path
import plistlib
import sqlite3
import sys
import tempfile
import unittest
from unittest.mock import patch


TOOLS = Path(__file__).resolve().parents[1] / 'tools'
sys.path.insert(0, str(TOOLS))
spec = importlib.util.spec_from_file_location('observer', TOOLS / 'observe_app_resources.py')
o = importlib.util.module_from_spec(spec)
spec.loader.exec_module(o)
SOURCE = '8' * 40


def clock(seconds=0, *, awake=None, boot='boot-one'):
    return {'wall_ns': 1_800_000_000_000_000_000 + int(seconds * 1e9),
            'utc': o.utc(1_800_000_000_000_000_000 + int(seconds * 1e9)),
            'boot_uuid': boot, 'sleep_wake_uuid': 'power-one',
            'awake_ns': int((1000 + (seconds if awake is None else awake)) * 1e9),
            'continuous_ns': int((1000 + seconds) * 1e9)}


def roles(cpu=0):
    return {role: {'status': 'known', 'pid': 10 + i, 'parent': 1 if i == 0 else 10,
                   'identity_key': 'identity-' + role, 'cpu_ns': int(cpu * 1e9),
                   'rss_bytes': 1000 + i, 'footprint_bytes': 900 + i}
            for i, role in enumerate(o.ROLES)}


def endpoint(seconds=0, cpu=0, session='one'):
    return {'clock': clock(seconds), 'roles': roles(cpu), 'collector_session': session,
            'source_status': 'match', 'elapsed_seconds': seconds,
            'elapsed_clock_basis': 'continuous_same_boot', 'database_pin_status': 'match'}


class FakeProbe:
    def __init__(self):
        self.clock = clock()
        self.rows = roles()
        self.process_calls = 0

    def clocks(self):
        return copy.deepcopy(self.clock)

    def processes(self, _app, _clock):
        self.process_calls += 1
        return copy.deepcopy(self.rows), []

    def usage(self, _pid):
        return {'start_abstime': 12345}


class IntervalTests(unittest.TestCase):
    def test_same_start_identity_cpu_uses_awake_duration(self):
        span = o.interval_between(endpoint(), endpoint(60, 3), 60)
        self.assertEqual(span['barriers'], [])
        self.assertEqual(span['roles']['gui']['cpu_seconds'], 3)
        self.assertEqual(span['roles']['gui']['average_cpu_percent_one_core'], 5)

    def test_pid_reuse_does_not_join_and_other_roles_remain_independent(self):
        a, b = endpoint(), endpoint(60, 2)
        b['roles']['serve']['identity_key'] = 'same-pid-new-start-cookie'
        span = o.interval_between(a, b, 60)
        self.assertEqual(span['roles']['serve']['reason'], 'process_identity_changed')
        self.assertEqual(span['roles']['watch']['status'], 'measured')

    def test_sleep_gap_boot_and_collector_boundaries_never_count_cpu(self):
        a = endpoint()
        sleeping = endpoint(60, 10)
        sleeping['clock'] = clock(60, awake=10)
        cases = [(sleeping, 'sleep_observed'), (endpoint(120, 10), 'sampling_gap'),
                 (endpoint(60, 10, 'new-collector'), 'collector_session_changed')]
        boot = endpoint(60, 10)
        boot['clock']['boot_uuid'] = 'new-boot'
        cases.append((boot, 'boot_changed'))
        for b, reason in cases:
            with self.subTest(reason=reason):
                span = o.interval_between(a, b, 60)
                self.assertIn(reason, span['barriers'])
                self.assertTrue(all(x['status'] == 'unknown' for x in span['roles'].values()))

    def test_missing_clock_negative_cpu_and_unknown_runtime_are_not_zero(self):
        a, b = endpoint(0, 5), endpoint(60, 4)
        span = o.interval_between(a, b, 60)
        self.assertEqual(span['roles']['gui']['reason'], 'negative_cpu_delta')
        self.assertNotIn('cpu_seconds', span['roles']['gui'])
        b['roles']['gui'] = {'status': 'unknown'}
        self.assertEqual(o.interval_between(a, b, 60)['roles']['gui']['reason'], 'runtime_unknown_or_missing')
        b['clock']['awake_ns'] = None
        self.assertIn('clock_unknown', o.interval_between(a, b, 60)['barriers'])
        b['clock'] = clock(60, boot=None)
        self.assertIn('boot_unknown', o.interval_between(a, b, 60)['barriers'])

    def test_wall_adjustment_and_backwards_clock_are_barriers(self):
        a, b = endpoint(), endpoint(60, 1)
        b['clock']['wall_ns'] += 10_000_000_000
        self.assertIn('wall_clock_adjustment', o.interval_between(a, b, 60)['barriers'])
        b['clock'] = clock(-1)
        self.assertIn('clock_non_monotonic', o.interval_between(a, b, 60)['barriers'])

    def test_unverified_database_identity_is_not_a_normal_interval(self):
        a, b = endpoint(), endpoint(60, 1)
        b['database_pin_status'] = 'unknown'
        span = o.interval_between(a, b, 60)
        self.assertIn('database_identity_unverified', span['barriers'])
        self.assertEqual(span['roles']['serve']['status'], 'unknown')

    def test_sleep_inside_capture_invalidates_adjacent_cpu_intervals(self):
        barriers = o.capture_barriers(clock(), clock(65, awake=5), 60)
        self.assertEqual(barriers, ['capture_sleep_observed'])
        a, b = endpoint(), endpoint(60, 1)
        b['capture_barriers'] = barriers
        self.assertIn('endpoint_capture_unverified', o.interval_between(a, b, 60)['barriers'])
        self.assertIn('endpoint_capture_unverified', o.interval_between(b, endpoint(120, 2), 60)['barriers'])

    def test_subsecond_sleep_and_power_uuid_changes_are_conservative_barriers(self):
        a, b = endpoint(), endpoint(60, 1)
        b['clock'] = clock(60, awake=60 - 0.393167)
        span = o.interval_between(a, b, 60)
        self.assertIn('sleep_observed', span['barriers'])
        self.assertEqual(span['roles']['gui']['status'], 'unknown')
        self.assertIn('capture_sleep_observed', o.capture_barriers(clock(), b['clock'], 60))
        b['clock'] = clock(60)
        b['clock']['sleep_wake_uuid'] = 'power-two'
        span = o.interval_between(a, b, 60)
        self.assertIn('power_transition', span['barriers'])
        self.assertNotIn('sleep_observed', span['barriers'])
        self.assertIn('capture_power_transition', o.capture_barriers(clock(), b['clock'], 60))
        b['clock']['sleep_wake_uuid'] = None
        self.assertIn('power_state_unknown', o.interval_between(a, b, 60)['barriers'])
        self.assertIn('capture_power_state_unknown', o.capture_barriers(clock(), b['clock'], 60))


class NativeInventoryTests(unittest.TestCase):
    def test_unresolved_same_bundle_cli_prevents_claiming_unique_role(self):
        probe = object.__new__(o.NativeProbe)
        app = Path('/fake/Test App.app')
        gui, cli = (str(app / 'Contents/MacOS' / p) for p in ('MacEveryApp', 'macevery'))

        def usage(pid):
            if pid == 12:
                raise o.ObserverError('injected unavailable process identity')
            return {'start_abstime': pid * 100, 'uuid': 'uuid-' + str(pid),
                    'cpu_ns': 100, 'rss_bytes': 1000, 'footprint_bytes': 900}

        def command(args, **kwargs):
            self.assertEqual(kwargs['errors'], 'replace')
            if args[-1] == 'pid=,ppid=,comm=':
                return f'10 1 {gui}\n11 10 {cli}\n12 10 {cli}\n'
            return cli + ' serve --token secret-must-never-be-persisted'

        with patch.object(probe, 'usage', side_effect=usage), \
                patch.object(probe, 'executable', side_effect=lambda pid: gui if pid == 10 else cli), \
                patch.object(o.subprocess, 'check_output', side_effect=command):
            result, untracked = probe.processes(app, clock())
        self.assertEqual(result['gui']['status'], 'known')
        self.assertEqual(result['serve']['status'], 'unknown')
        self.assertEqual(result['serve']['reason'], 'unresolved_cli_identity')
        self.assertEqual(result['watch']['status'], 'unknown')
        self.assertEqual(untracked[0]['pid'], 12)
        self.assertNotIn('secret-must-never-be-persisted', json.dumps([result, untracked]))


class StorageTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix='macevery-observer-tests-')
        self.root = Path(self.scratch.name)
        self.app = self.root / 'Test App.app'
        contents = self.app / 'Contents'
        (contents / 'MacOS').mkdir(parents=True)
        (contents / 'Info.plist').write_bytes(plistlib.dumps({'MacEveryGitCommit': SOURCE,
                                                          'MacEveryBuildLabel': 'fixture'}))
        (contents / 'MacOS/MacEveryApp').write_bytes(b'GUI fixture, never executed')
        (contents / 'MacOS/macevery').write_bytes(b'CLI fixture, never executed')
        self.database = self.root / 'owned.sqlite'
        with sqlite3.connect(self.database) as c:
            c.executescript("CREATE TABLE macevery_content_state(singleton INTEGER PRIMARY KEY,database_id TEXT,revision INTEGER,journal_floor INTEGER);"
                            "INSERT INTO macevery_content_state VALUES(1,'fixture-db',10,0);"
                            "CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT);"
                            "INSERT INTO meta VALUES('last_indexed_at','123');")
        self.directory = self.root / 'state'
        self.probe = FakeProbe()
        self.manifest, self.state = o.initialize(self.directory, self.app, self.database, SOURCE,
                                               hours=1, interval=60, probe=self.probe)

    def tearDown(self):
        self.scratch.cleanup()

    def take(self):
        with o.writer_lock(self.directory):
            self.manifest, self.state = o.load_state(self.directory)
            return o.capture(self.directory, self.manifest, self.state, self.probe,
                             self.manifest['manual_session'])

    def test_private_state_jsonl_and_read_only_meta(self):
        original = self.database.read_bytes()
        a = self.take()
        self.probe.clock = clock(60)
        self.probe.rows = roles(1)
        b = self.take()
        self.assertEqual([r['sequence'] for r in o.read_records(self.directory)], [1, 2])
        self.assertEqual(b['database']['revision'], 10)
        self.assertEqual(b['content_revision_delta'], 0)
        self.assertEqual(self.database.read_bytes(), original)
        self.assertEqual(self.directory.stat().st_mode & 0o777, 0o700)
        self.assertEqual((self.directory / 'state.json').stat().st_mode & 0o777, 0o600)
        self.assertEqual(a['interval']['barriers'], ['initial_endpoint'])
        self.assertEqual(a['database_pin_status'], 'match')
        self.assertEqual(self.manifest['initial_database']['database_id'], 'fixture-db')
        self.assertEqual(a['content_revision_delta'], 0)

    def test_first_endpoint_checks_initial_database_and_does_not_repin(self):
        with sqlite3.connect(self.database) as c:
            c.execute("UPDATE macevery_content_state SET database_id='replacement-db'")
        a = self.take()
        self.assertEqual(a['database_pin_status'], 'mismatch')
        self.assertIn('initial_database_pin_mismatch', a['database_events'])
        self.assertEqual(self.manifest['initial_database']['database_id'], 'fixture-db')
        self.probe.clock = clock(60)
        b = self.take()
        self.assertEqual(b['database_pin_status'], 'mismatch')
        self.assertIn('database_identity_unverified', b['interval']['barriers'])

    def test_same_database_id_with_replaced_inode_is_a_mismatch(self):
        replacement = self.root / 'replacement.sqlite'
        replacement.write_bytes(self.database.read_bytes())
        os.replace(replacement, self.database)
        row = self.take()
        self.assertEqual(row['database']['database_id'], 'fixture-db')
        self.assertEqual(row['database_pin_status'], 'mismatch')

    def test_unknown_initial_or_later_database_is_persisted_not_adopted(self):
        unknown_state = self.root / 'unknown-initial-state'
        with patch.object(o, 'database_meta', side_effect=sqlite3.OperationalError('injected')):
            manifest, state = o.initialize(unknown_state, self.app, self.database, SOURCE,
                                           hours=1, probe=self.probe)
        self.assertEqual(manifest['initial_database']['status'], 'unknown')
        row = o.capture(unknown_state, manifest, state, self.probe, 'manual')
        self.assertEqual(row['database']['status'], 'known')
        self.assertEqual(row['database_pin_status'], 'unknown')
        self.assertIn('initial_database_pin_unknown', row['database_events'])
        self.assertEqual(manifest['initial_database']['status'], 'unknown')
        self.take()
        self.probe.clock = clock(60)
        with patch.object(o, 'database_meta', side_effect=sqlite3.OperationalError('injected')):
            row = self.take()
        self.assertEqual(row['database']['status'], 'unknown')
        self.assertEqual(row['database_pin_status'], 'unknown')
        self.assertIn('database_identity_unverified', row['interval']['barriers'])
        report = o.summarize_records(self.manifest, o.read_records(self.directory), self.state)
        self.assertEqual(report['database_pin_status_counts'], {'match': 1, 'unknown': 1})

    def test_nonfinite_hours_and_interval_never_initialize(self):
        untouched = self.root / 'invalid-duration-state'
        for hours, interval in [(float('nan'), 60), (float('inf'), 60),
                                (1, float('nan')), (1, float('inf'))]:
            with self.subTest(hours=hours, interval=interval), self.assertRaises(o.ObserverError):
                o.initialize(untouched, self.app, self.database, SOURCE,
                             hours=hours, interval=interval, probe=self.probe)
            self.assertFalse(untouched.exists())

    def test_failed_atomic_replace_keeps_previous_json(self):
        target = self.directory / 'cache.json'
        o.atomic_json(target, {'version': 1})
        with patch.object(o.os, 'replace', side_effect=OSError('injected')):
            with self.assertRaises(OSError):
                o.atomic_json(target, {'version': 2})
        self.assertEqual(json.loads(target.read_text()), {'version': 1})
        self.assertEqual(list(self.directory.glob('.cache.json-*')), [])

    def test_partial_append_is_preserved_and_refuses_automatic_continuation(self):
        journal = self.directory / 'samples.jsonl'
        original_write = os.write
        with patch.object(o.os, 'write', side_effect=lambda fd, b: original_write(fd, b[:-1])):
            with self.assertRaises(o.ObserverError):
                o.append_record(journal, {'sequence': 1})
        partial = journal.read_bytes()
        self.assertTrue(partial and not partial.endswith(b'\n'))
        with self.assertRaises(o.ObserverError):
            o.load_state(self.directory)
        self.assertEqual(journal.read_bytes(), partial)

    def test_recover_append_committed_before_state(self):
        original = o.atomic_json

        def fail_state(path, value):
            if Path(path).name == 'state.json':
                raise OSError('injected after JSONL fsync')
            return original(path, value)

        with patch.object(o, 'atomic_json', side_effect=fail_state):
            with self.assertRaises(OSError):
                self.take()
        _, recovered = o.load_state(self.directory)
        self.assertEqual(recovered['sequence'], 1)
        self.assertTrue(recovered['recovered_append_before_state'])
        self.probe.clock = clock(60)
        self.probe.rows = roles(1)
        self.assertEqual(self.take()['sequence'], 2)

    def test_truncated_committed_history_never_silently_rolls_back(self):
        self.take()
        journal = self.directory / 'samples.jsonl'
        first_endpoint = journal.read_bytes()
        self.probe.clock = clock(60)
        self.take()
        state_path = self.directory / 'state.json'
        committed_state = state_path.read_bytes()
        journal.write_bytes(first_endpoint)
        with self.assertRaisesRegex(o.ObserverError, 'truncated'):
            o.load_state(self.directory)
        self.assertEqual(state_path.read_bytes(), committed_state)
        self.assertEqual(journal.read_bytes(), first_endpoint)

    def test_expected_source_mismatch_never_initializes_or_samples_wrong_app(self):
        untouched = self.root / 'should-not-exist'
        with self.assertRaises(o.ObserverError):
            o.initialize(untouched, self.app, self.database, '9' * 40, probe=self.probe)
        self.assertFalse(untouched.exists())
        (self.app / 'Contents/MacOS/macevery').write_bytes(b'different payload')
        row = self.take()
        self.assertEqual(row['source_status'], 'mismatch')
        self.assertEqual(self.probe.process_calls, 0)
        self.assertTrue(all(r['status'] == 'unknown' for r in row['roles'].values()))

    def test_tool_fingerprint_change_refuses_reuse(self):
        with patch.object(o, 'file_hash', return_value='changed'):
            with self.assertRaises(o.ObserverError):
                o.load_state(self.directory)

    def test_metrics_helper_fingerprint_change_refuses_reuse(self):
        original = o.file_hash
        with patch.object(o, 'file_hash', side_effect=lambda p: 'changed' if Path(p) == o.METRICS_HELPER else original(p)):
            with self.assertRaises(o.ObserverError):
                o.load_state(self.directory)

    def test_summary_splits_process_identities_and_does_not_claim_complete_coverage(self):
        self.take()
        self.probe.clock = clock(60)
        self.probe.rows = roles(1)
        self.take()
        self.probe.clock = clock(180)
        self.probe.rows['serve']['identity_key'] = 'replacement-start'
        self.probe.rows['serve']['cpu_ns'] = 0
        self.take()
        report = o.summarize_records(self.manifest, o.read_records(self.directory), self.state)
        self.assertEqual(report['roles']['serve']['observed_identity_count'], 2)
        self.assertEqual(len(report['roles']['serve']['process_identities']), 2)
        self.assertEqual(report['roles']['serve']['measured_awake_seconds'], 60)
        self.assertEqual(report['barrier_counts']['sampling_gap'], 1)
        self.assertFalse(report['window_finished'])
        encoded = json.dumps(report)
        self.assertNotIn(str(self.root), encoded)
        self.assertNotIn('fixture-db', encoded)

    def test_boot_change_uses_explicit_unknown_continuity_not_a_new_window(self):
        self.probe.clock = clock(1800, boot='different-boot')
        row = self.take()
        self.assertEqual(row['elapsed_seconds'], 1800)
        self.assertEqual(row['elapsed_clock_basis'], 'wall_fallback_unknown_continuity')
        self.assertEqual(self.manifest['duration_seconds'], 3600)

    def test_run_closes_existing_absolute_window_without_wait_or_app_actions(self):
        self.probe.clock = clock(3601)
        with patch.object(o.threading.Event, 'wait', side_effect=AssertionError('must not wait')):
            o.run_observer(self.directory, self.probe)
        _, state = o.load_state(self.directory)
        self.assertTrue(state['window_finished'])
        self.assertEqual(state['last_exit']['reason'], 'window_elapsed')
        self.assertEqual(state['worker']['status'], 'exited')
        with self.assertRaises(o.ObserverError):
            o.capture(self.directory, self.manifest, state, self.probe, 'late')

    def test_second_writer_is_rejected(self):
        with o.writer_lock(self.directory):
            with self.assertRaises(o.ObserverError):
                with o.writer_lock(self.directory):
                    pass


if __name__ == '__main__':
    unittest.main()
