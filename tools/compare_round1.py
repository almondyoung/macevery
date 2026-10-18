#!/usr/bin/env python3
"""Compile the same driver against two rlibs and run sequential A/B or A/B/A blocks.

Examples (all raw evidence stays in --output; the live index is never opened):
  python3 tools/compare_round1.py --baseline-lib /tmp/base/release/libmacevery.rlib \
    --candidate-lib /tmp/new/release/libmacevery.rlib --rows 100000 --rounds 3 --order aba \
    --output /private/tmp/macevery-synthetic-100k
  # First real-data pass only, one fresh process per variant, constant memory backend:
  python3 tools/compare_round1.py --baseline-lib ... --candidate-lib ... \
    --dataset snapshot --snapshot /private/tmp/macevery-round1-20261017/snapshot.sqlite \
    --backend memory --rounds 1 --order ab --output /private/tmp/macevery-real-first

Each snapshot process reads its own APFS clone (ordinary copy fallback). The original
snapshot is never passed to Database::open. Clone time is excluded from load timing.
Process CPU comes from child getrusage; external RSS sampling is outside that process.
RSS sampling can miss short-lived peaks. Per-query phase RSS is an estimate:
asynchronous stdout delivery can include result encoding just after the scorer.
Legacy fields named query_*_lower_bound_bytes are not pure-query allocation bounds.
Process high-water RSS and load-end RSS are the primary memory comparisons.
No allocator instrumentation is used. Kernel file caches are not flushed; load timings
are warm-cache observations. Full ordered result records are SHA-256 compared per query.
"""
import argparse
import ctypes
import datetime as dt
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def write_json(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")


def stat_identity(path):
    s = path.stat()
    return {"path": str(path), "size": s.st_size, "mtime_ns": s.st_mtime_ns, "inode": s.st_ino}


class RssReader:
    def __init__(self):
        self.lib = None
        if sys.platform == "darwin":
            self.lib = ctypes.CDLL("/usr/lib/libproc.dylib")
            self.lib.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
            self.lib.proc_pidinfo.restype = ctypes.c_int

    def read(self, pid):
        try:
            if self.lib:
                buf = (ctypes.c_uint64 * 12)()
                if self.lib.proc_pidinfo(pid, 4, 0, buf, 96) == 96:
                    return int(buf[1])
            elif sys.platform.startswith("linux"):
                for line in Path(f"/proc/{pid}/status").read_text().splitlines():
                    if line.startswith("VmRSS:"):
                        return int(line.split()[1]) * 1024
        except (FileNotFoundError, ProcessLookupError):
            pass
        return None


def clone_snapshot(src, dst):
    if sys.platform == "darwin":
        result = subprocess.run(["/bin/cp", "-c", str(src), str(dst)], capture_output=True, text=True)
        if result.returncode == 0:
            return "apfs_clone"
        dst.unlink(missing_ok=True)
    shutil.copyfile(src, dst)
    return "ordinary_copy"


def run_one(args, driver, variant, block, ordinal, out):
    run_id = f"run-{ordinal:02d}-{variant}"
    metadata = {"run_id": run_id, "variant": variant, "block": block, "ordinal": ordinal}
    backend = (args.baseline_backend if variant == "baseline" else args.candidate_backend) or args.backend
    metadata["storage_backend"] = backend
    with tempfile.TemporaryDirectory(prefix=run_id + "-", dir=out) as scratch:
        db = Path(scratch) / "index.sqlite"
        if args.dataset == "snapshot":
            metadata["snapshot_copy_mode"] = clone_snapshot(args.snapshot, db)
        command = [str(driver), "--dataset", args.dataset, "--backend", backend,
                   "--rows", str(args.rows), "--db", str(db), "--query-rounds", str(args.query_rounds),
                   "--fixed-now", str(args.fixed_now), "--limit", str(args.limit), "--queries", args.queries]
        metadata["command"] = command
        events, samples = [], []
        active, sampled_peaks = [None], {}
        lock, stop = threading.Lock(), threading.Event()
        reader = RssReader()
        started = time.monotonic()
        with (out / f"{run_id}.stderr.txt").open("w") as err, (out / f"{run_id}.jsonl").open("w") as raw:
            process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=err, text=True, bufsize=1)
            metadata["pid"] = process.pid

            def sample():
                while not stop.is_set():
                    rss = reader.read(process.pid)
                    elapsed = (time.monotonic() - started) * 1000
                    with lock:
                        phase = active[0]
                        if rss is not None:
                            samples.append([round(elapsed, 3), rss, phase])
                            if phase is not None:
                                sampled_peaks[phase] = max(sampled_peaks.get(phase, 0), rss)
                    stop.wait(args.sample_ms / 1000)

            sampler = threading.Thread(target=sample, daemon=True)
            sampler.start()
            try:
                for line in process.stdout:
                    raw.write(line)
                    raw.flush()
                    event = json.loads(line)
                    events.append(event)
                    key = f"{event.get('id')}:{event.get('iteration')}"
                    with lock:
                        if event["event"] == "query_start":
                            active[0] = key
                            sampled_peaks[key] = event.get("rss_before_bytes") or 0
                        elif event["event"] == "query_end":
                            sampled_peaks[key] = max(sampled_peaks.get(key, 0), event.get("rss_after_bytes") or 0)
                            active[0] = None
                metadata["exit_code"] = process.wait()
            finally:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
                stop.set()
                sampler.join()
        metadata["process_wall_ms"] = (time.monotonic() - started) * 1000
        metadata["events"] = events
        with (out / f"{run_id}.rss.jsonl").open("w") as f:
            for t_ms, rss, phase in samples:
                f.write(json.dumps({"t_ms": t_ms, "rss_bytes": rss, "query_phase": phase}) + "\n")
        metadata["samples"] = len(samples)
        for event in events:
            if event["event"] == "query_end":
                key = f"{event['id']}:{event['iteration']}"
                peak = sampled_peaks.get(key, event.get("rss_before_bytes") or 0)
                if event["process_peak_rss_bytes"] > event["process_peak_rss_before_bytes"]:
                    peak = max(peak, event["process_peak_rss_bytes"])
                # Keep the existing schema, but these are phase-attributed RSS
                # estimates, not strict bounds on allocations inside the scorer.
                event["query_peak_rss_lower_bound_bytes"] = peak
                event["query_transient_rss_lower_bound_bytes"] = max(0, peak - (event.get("rss_before_bytes") or 0))
            elif event["event"] == "results":
                canonical = json.dumps(event["results"], sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode()
                event["sha256"] = hashlib.sha256(canonical).hexdigest()
        write_json(out / f"{run_id}.combined.json", metadata)
        if metadata["exit_code"]:
            raise RuntimeError(f"{run_id} failed: see {out / (run_id + '.stderr.txt')}")
        required = {"start", "load_end", "query_end", "results", "process_end"}
        if not required.issubset({e["event"] for e in events}):
            raise RuntimeError(f"{run_id}: incomplete event stream")
        print(f"{run_id}: complete, {len(samples)} RSS samples", file=sys.stderr, flush=True)
        return metadata


def describe(values):
    values = sorted(values)
    return {"n": len(values), "median": statistics.median(values), "min": min(values),
            "max": max(values), "p95": values[max(0, math.ceil(len(values) * .95) - 1)], "values": values}


def comparison(baseline, candidate):
    a, b = statistics.median(baseline), statistics.median(candidate)
    return {"baseline": describe(baseline), "candidate": describe(candidate),
            "candidate_change_pct": ((b / a) - 1) * 100 if a else None,
            "reduction_pct": (1 - b / a) * 100 if a else None}


def summarize(runs):
    digests, metrics = {}, {}
    for run in runs:
        variant = run["variant"]
        for event in run["events"]:
            if event["event"] == "results":
                digests.setdefault(event["id"], {}).setdefault(event["sha256"], []).append(
                    {"run_id": run["run_id"], "iteration": event["iteration"], "count": len(event["results"])})
            fields = {
                "load_end": ["wall_ms", "cpu_ms", "rss_after_bytes"],
                "query_end": ["wall_ms", "cpu_ms", "query_peak_rss_lower_bound_bytes", "query_transient_rss_lower_bound_bytes"],
                "process_end": ["process_peak_rss_bytes"],
            }.get(event["event"], [])
            prefix = event["event"] + (":" + event["id"] if "id" in event else "")
            for field in fields:
                if event.get(field) is not None:
                    metrics.setdefault(prefix + ":" + field, {}).setdefault(variant, []).append(event[field])
        for field in ["wall_ms", "cpu_ms"]:
            total = sum(event[field] for event in run["events"] if event["event"] == "query_end")
            metrics.setdefault("query_group:" + field, {}).setdefault(variant, []).append(total)
    mismatches = {query: hashes for query, hashes in digests.items() if len(hashes) != 1}
    # Each block compares its candidate against the median of surrounding A runs.
    paired = {}
    for block in sorted({r["block"] for r in runs}):
        subset = [r for r in runs if r["block"] == block]
        block_summary = summarize_metrics(subset)
        for key, value in block_summary.items():
            if value["candidate_change_pct"] is not None:
                paired.setdefault(key, []).append(value["candidate_change_pct"])
    return {"digest_match": not mismatches, "digest_mismatches": mismatches, "digests": digests,
            "metrics": {key: comparison(v["baseline"], v["candidate"]) for key, v in metrics.items()
                        if "baseline" in v and "candidate" in v},
            "paired_block_candidate_change_pct": {key: describe(values) for key, values in paired.items()},
            "notes": ["Negative candidate_change_pct means lower usage/time; reduction_pct is positive for improvements.",
                      "RSS is resident bytes; query phase estimates may miss brief peaks or include output encoding.",
                      "Legacy query_*_lower_bound_bytes names do not imply pure-query allocation bounds.",
                      "Process peak RSS includes dataset preparation, loading, searching and result serialization.",
                      "No allocator counters; external sampler CPU is excluded from child getrusage CPU.",
                      "Cache state is uncontrolled; repeated queries and A-B-A runs expose warm-cache behavior."]}


def summarize_metrics(runs):
    metrics = {}
    for run in runs:
        for event in run["events"]:
            fields = {"load_end": ["wall_ms", "cpu_ms", "rss_after_bytes"],
                      "query_end": ["wall_ms", "cpu_ms", "query_peak_rss_lower_bound_bytes", "query_transient_rss_lower_bound_bytes"],
                      "process_end": ["process_peak_rss_bytes"]}.get(event["event"], [])
            prefix = event["event"] + (":" + event["id"] if "id" in event else "")
            for field in fields:
                if event.get(field) is not None:
                    metrics.setdefault(prefix + ":" + field, {}).setdefault(run["variant"], []).append(event[field])
        for field in ["wall_ms", "cpu_ms"]:
            total = sum(event[field] for event in run["events"] if event["event"] == "query_end")
            metrics.setdefault("query_group:" + field, {}).setdefault(run["variant"], []).append(total)
    return {key: comparison(v["baseline"], v["candidate"]) for key, v in metrics.items()
            if "baseline" in v and "candidate" in v}


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--baseline-lib", type=Path, required=True)
    p.add_argument("--candidate-lib", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--dataset", choices=["synthetic", "snapshot"], default="synthetic")
    p.add_argument("--snapshot", type=Path)
    p.add_argument("--backend", choices=["memory", "compact"], default="memory")
    p.add_argument("--baseline-backend", choices=["memory", "compact"],
                   help="Override baseline storage; round 2 compares full records with packed storage")
    p.add_argument("--candidate-backend", choices=["memory", "compact"],
                   help="Override candidate storage; both are compared against complete ordered results")
    p.add_argument("--rows", type=int, default=100_000)
    p.add_argument("--rounds", type=int, default=3)
    p.add_argument("--order", choices=["ab", "aba"], default="aba")
    p.add_argument("--query-rounds", type=int, default=1)
    p.add_argument("--queries", default="broad,plain,nohit,glob,fuzzy,filters,mtime,unicode,path")
    p.add_argument("--fixed-now", type=int, default=1792195200)
    p.add_argument("--limit", type=int, default=80)
    p.add_argument("--sample-ms", type=float, default=1)
    args = p.parse_args()
    if min(args.rows, args.rounds, args.query_rounds) <= 0 or args.sample_ms <= 0 or args.limit < 0:
        p.error("rows/rounds/query-rounds/sample-ms must be positive; limit must be nonnegative")
    if args.dataset == "snapshot":
        if not args.snapshot:
            p.error("snapshot dataset requires --snapshot")
        args.snapshot = args.snapshot.resolve(strict=True)
        for suffix in ["-wal", "-journal"]:
            sidecar = Path(str(args.snapshot) + suffix)
            if sidecar.exists() and sidecar.stat().st_size:
                p.error("snapshot has a nonempty journal/WAL; freeze it with SQLite online backup first")
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=True)
    if (out / "manifest.json").exists():
        p.error("output already contains a manifest; choose a new output to preserve evidence")
    tools = Path(__file__).resolve().parent
    manifest = {"created_at_utc": dt.datetime.now(dt.timezone.utc).isoformat(), "arguments": vars(args).copy(),
                "platform": platform.platform(), "machine": platform.machine(), "logical_cpus": os.cpu_count(),
                "python": sys.version, "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
                "driver_compile_flags": ["--edition=2021", "-O", "-C", "lto=fat"],
                "driver_sha256": sha256(tools / "perf_round1.rs"), "wrapper_sha256": sha256(Path(__file__)),
                "snapshot_identity": stat_identity(args.snapshot) if args.snapshot else None,
                "libraries": {variant: {"identity": stat_identity(path.resolve(strict=True)), "sha256": sha256(path)}
                              for variant, path in [("baseline", args.baseline_lib), ("candidate", args.candidate_lib)]}}
    manifest["arguments"] = {key: str(value) if isinstance(value, Path) else value for key, value in manifest["arguments"].items()}
    write_json(out / "manifest.json", manifest)
    drivers = {}
    for variant, lib in [("baseline", args.baseline_lib), ("candidate", args.candidate_lib)]:
        binary = out / f"perf-{variant}"
        command = ["rustc", "--edition=2021", "-O", "-C", "lto=fat", str(tools / "perf_round1.rs"), "--extern", f"macevery={lib.resolve()}", "-o", str(binary)]
        subprocess.run(command, check=True)
        drivers[variant] = binary
    runs = []
    try:
        for block in range(args.rounds):
            for variant in (["baseline", "candidate", "baseline"] if args.order == "aba" else ["baseline", "candidate"]):
                runs.append(run_one(args, drivers[variant], variant, block, len(runs), out))
                write_json(out / "runs.json", runs)
                if args.snapshot and stat_identity(args.snapshot) != manifest["snapshot_identity"]:
                    raise RuntimeError("source snapshot changed during benchmarking")
        summary = summarize(runs)
        write_json(out / "summary.json", summary)
        print(json.dumps({"output": str(out), "digest_match": summary["digest_match"], "processes": len(runs)}, ensure_ascii=False))
        return 0 if summary["digest_match"] else 2
    except Exception as error:
        write_json(out / "failure.json", {"error": str(error), "completed_runs": len(runs)})
        raise


if __name__ == "__main__":
    sys.exit(main())
