#!/usr/bin/env python3
"""Compile and run the transaction concurrency-control benchmark matrix."""

from __future__ import annotations

import argparse
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import time
from typing import Any, Sequence

try:
    from scripts import transaction_cc_bench_report as reporter
except ModuleNotFoundError:
    import transaction_cc_bench_report as reporter


SCHEMA_VERSION = 1
REPOSITORY_ROOT = Path(__file__).resolve().parents[1]

BASE_FEATURES = (
    "anyhow",
    "async",
    "backtrace",
    "cache",
    "gc",
    "gc-copying",
    "gc-drc",
    "gc-null",
    "wat",
    "profiling",
    "parallel-compilation",
    "cranelift",
    "pooling-allocator",
    "demangle",
    "addr2line",
    "coredump",
    "debug-builtins",
    "runtime",
    "component-model",
    "component-model-async",
    "threads",
    "stack-switching",
    "std",
    "debug",
    "compile-time-builtins",
    "wit-parser",
)

POLICIES = {
    "lockbased": ["transaction-cc-lockbased"],
    "no-wait": ["transaction-cc-nowait-abort"],
    "optimistic": ["transaction-cc-optimistic-validation"],
    "strict-2pl": ["transaction-cc-strict-2pl"],
    "timestamp": ["transaction-cc-timestamp-ordering"],
    "wait-die": ["transaction-cc-wait-die"],
    "wound-wait": ["transaction-cc-wound-wait"],
    "mvcc-optimistic": [
        "transaction-mvcc",
        "transaction-cc-optimistic-validation",
    ],
}

BACKENDS = ("vmemory", "file-backed")
WORKLOADS = ("read-only", "disjoint-writes", "hot-key", "write-skew")
DEFAULT_WORKERS = (1, 2, 4, 8, 16)
QUICK_WARMUP_MS = 100
QUICK_MEASURE_MS = 500
DEFAULT_SEED = 0x5EED5EEDD15CA11E
DEFAULT_GC_COMMIT_INTERVAL = 4096


@dataclass(frozen=True)
class PolicyResult:
    policy: str
    returncode: int
    command: list[str]
    request_path: str
    output_path: str
    started_utc: str
    ended_utc: str
    elapsed_seconds: float
    driver_complete: bool
    error: str | None = None

    @property
    def succeeded(self) -> bool:
        return self.returncode == 0 and self.driver_complete and self.error is None


def features_for(policy: str) -> list[str]:
    """Return the complete, exact feature closure for one policy build."""
    try:
        policy_features = POLICIES[policy]
    except KeyError as error:
        raise ValueError(f"unknown policy {policy!r}") from error
    features = [*BASE_FEATURES, *policy_features]
    cc_features = [feature for feature in features if feature.startswith("transaction-cc-")]
    if len(cc_features) != 1:
        raise AssertionError(f"policy {policy!r} selected {len(cc_features)} CC features")
    if policy == "mvcc-optimistic":
        if "transaction-mvcc" not in features:
            raise AssertionError("MVCC policy omitted transaction-mvcc")
    elif "transaction-mvcc" in features:
        raise AssertionError(f"single-version policy {policy!r} selected MVCC")
    return features


def build_command(policy: str, request: Path, output: Path) -> list[str]:
    """Build the release-test command for a policy.

    Request and output are transported through the driver's environment. They
    remain parameters here so callers cannot accidentally construct a command
    without first assigning both artifacts.
    """
    del request, output
    return [
        "cargo",
        "test",
        "-q",
        "-p",
        "wasmtime",
        "--release",
        "--no-default-features",
        "--features",
        ",".join(features_for(policy)),
        "transaction_cc_benchmark_driver",
        "--lib",
        "--",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]


def _comma_values(value: str, allowed: Sequence[str], label: str) -> list[str]:
    values = value.split(",")
    if not values or any(not item for item in values):
        raise argparse.ArgumentTypeError(f"{label} must be a nonempty comma-list")
    unknown = [item for item in values if item not in allowed]
    if unknown:
        raise argparse.ArgumentTypeError(
            f"unknown {label}: {','.join(unknown)}; expected one of {','.join(allowed)}"
        )
    if len(set(values)) != len(values):
        raise argparse.ArgumentTypeError(f"{label} must not contain duplicates")
    return values


def _policies(value: str) -> list[str]:
    if value == "all":
        return list(POLICIES)
    return _comma_values(value, tuple(POLICIES), "policies")


def _backends(value: str) -> list[str]:
    return _comma_values(value, BACKENDS, "backends")


def _workloads(value: str) -> list[str]:
    return _comma_values(value, WORKLOADS, "workloads")


def _positive_integer(value: str) -> int:
    try:
        parsed = int(value, 10)
    except ValueError as error:
        raise argparse.ArgumentTypeError(f"expected an integer, got {value!r}") from error
    if parsed <= 0:
        raise argparse.ArgumentTypeError("value must be greater than zero")
    return parsed


def _workers(value: str) -> list[int]:
    values = value.split(",")
    if not values or any(not item for item in values):
        raise argparse.ArgumentTypeError("workers must be a nonempty comma-list")
    parsed = [_positive_integer(item) for item in values]
    if len(set(parsed)) != len(parsed):
        raise argparse.ArgumentTypeError("workers must not contain duplicates")
    return parsed


def _seed(value: str) -> int:
    try:
        parsed = int(value, 0)
    except ValueError as error:
        raise argparse.ArgumentTypeError(
            f"seed must be a decimal or 0x-prefixed integer, got {value!r}"
        ) from error
    if not 0 <= parsed <= (1 << 64) - 1:
        raise argparse.ArgumentTypeError("seed must fit in an unsigned 64-bit integer")
    return parsed


def argument_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Run exact-feature transaction CC benchmark builds sequentially."
    )
    parser.add_argument("--profile", choices=("quick",), default="quick")
    parser.add_argument("--policies", type=_policies, default="all")
    parser.add_argument(
        "--backends", type=_backends, default=",".join(BACKENDS)
    )
    parser.add_argument(
        "--workloads", type=_workloads, default=",".join(WORKLOADS)
    )
    parser.add_argument(
        "--workers",
        type=_workers,
        default=",".join(str(workers) for workers in DEFAULT_WORKERS),
    )
    parser.add_argument("--warmup-ms", type=_positive_integer)
    parser.add_argument("--measure-ms", type=_positive_integer)
    parser.add_argument("--repetitions", type=_positive_integer, default=1)
    parser.add_argument("--seed", type=_seed, default=hex(DEFAULT_SEED))
    parser.add_argument(
        "--gc-commit-interval",
        type=_positive_integer,
        default=DEFAULT_GC_COMMIT_INTERVAL,
    )
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument("--skip-tfunc-control", action="store_true")
    return parser


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argument_parser()
    args = parser.parse_args(argv)
    if args.profile == "quick":
        if args.warmup_ms is None:
            args.warmup_ms = QUICK_WARMUP_MS
        if args.measure_ms is None:
            args.measure_ms = QUICK_MEASURE_MS
    return args


def request_for(policy: str, args: argparse.Namespace) -> dict[str, Any]:
    if policy not in POLICIES:
        raise ValueError(f"unknown policy {policy!r}")
    return {
        "schema_version": SCHEMA_VERSION,
        "expected_policy": policy,
        "warmup_ms": args.warmup_ms,
        "measure_ms": args.measure_ms,
        "repetitions": args.repetitions,
        "backends": list(args.backends),
        "workloads": list(args.workloads),
        "workers": list(args.workers),
        "seed": args.seed,
        "gc_commit_interval": args.gc_commit_interval,
        "include_tfunc_control": not args.skip_tfunc_control,
    }


def _write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    serialized = json.dumps(value, indent=2, sort_keys=True) + "\n"
    temporary.write_text(serialized, encoding="utf-8")
    temporary.replace(path)


def write_request(path: Path, request: dict[str, Any]) -> None:
    _write_json(path, request)


def _append_jsonl(path: Path, record: dict[str, Any]) -> None:
    with path.open("a", encoding="utf-8") as output:
        output.write(json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n")
        output.flush()
        os.fsync(output.fileno())


def _utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="microseconds").replace(
        "+00:00", "Z"
    )


def _complete_raw_stream(path: Path) -> tuple[bool, str | None]:
    try:
        with path.open(encoding="utf-8") as stream:
            last_line = None
            for line in stream:
                if line.strip():
                    last_line = line
        if last_line is None:
            return False, "driver produced an empty raw JSONL stream"
        record = json.loads(last_line)
        if record.get("record_type") != "complete":
            return False, "raw JSONL stream does not end with a complete record"
        return True, None
    except (OSError, json.JSONDecodeError, AttributeError) as error:
        return False, f"cannot validate raw JSONL completion: {error}"


def run_policy(
    policy: str, run_dir: Path, args: argparse.Namespace
) -> PolicyResult:
    if policy not in POLICIES:
        raise ValueError(f"unknown policy {policy!r}")
    request_dir = run_dir / "requests"
    raw_dir = run_dir / "raw"
    request_dir.mkdir(parents=True, exist_ok=True)
    raw_dir.mkdir(parents=True, exist_ok=True)
    request_path = request_dir / f"{policy}.json"
    output_path = raw_dir / f"{policy}.jsonl"
    write_request(request_path, request_for(policy, args))

    # Reserve the artifact before Cargo starts, but never reopen it in a mode
    # that can truncate data written by the Rust driver.
    with output_path.open("xb"):
        pass

    command = build_command(policy, request_path, output_path)
    environment = os.environ.copy()
    environment["WASMTIME_TRANSACTION_BENCH_REQUEST"] = str(request_path.resolve())
    environment["WASMTIME_TRANSACTION_BENCH_OUTPUT"] = str(output_path.resolve())
    started_utc = _utc_now()
    started = time.perf_counter()
    returncode = 127
    execution_error = None
    try:
        completed = subprocess.run(
            command,
            cwd=REPOSITORY_ROOT,
            env=environment,
            check=False,
        )
        returncode = completed.returncode
    except OSError as error:
        execution_error = f"failed to execute Cargo: {error}"
    elapsed = time.perf_counter() - started
    ended_utc = _utc_now()

    driver_complete = False
    completion_error = None
    if returncode == 0 and execution_error is None:
        driver_complete, completion_error = _complete_raw_stream(output_path)
        if not driver_complete:
            returncode = 1
    error = execution_error or completion_error
    if returncode != 0 and error is None:
        error = f"Cargo release-test exited with status {returncode}"
    return PolicyResult(
        policy=policy,
        returncode=returncode,
        command=command,
        request_path=str(request_path),
        output_path=str(output_path),
        started_utc=started_utc,
        ended_utc=ended_utc,
        elapsed_seconds=elapsed,
        driver_complete=driver_complete,
        error=error,
    )


def _capture(command: list[str]) -> str:
    completed = subprocess.run(
        command,
        cwd=REPOSITORY_ROOT,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if completed.returncode != 0:
        stderr = completed.stderr.strip()
        raise RuntimeError(
            f"{' '.join(command)} failed with status {completed.returncode}: {stderr}"
        )
    return completed.stdout.rstrip()


def _changed_paths() -> list[str]:
    status = _capture(["git", "status", "--short", "--untracked-files=all"])
    if not status:
        return []
    return sorted({line[3:] for line in status.splitlines() if len(line) >= 4})


def _cpu_model() -> str:
    processor = platform.processor().strip()
    if processor:
        return processor
    try:
        for line in Path("/proc/cpuinfo").read_text(encoding="utf-8").splitlines():
            if line.lower().startswith("model name") and ":" in line:
                return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return "unknown"


def _host_metadata(argv: Sequence[str], started_utc: str) -> dict[str, Any]:
    revision = _capture(["git", "rev-parse", "HEAD"])
    changed_paths = _changed_paths()
    uname = platform.uname()
    return {
        "schema_version": SCHEMA_VERSION,
        "git_revision": revision,
        "git_short_revision": revision[:12],
        "dirty": bool(changed_paths),
        "changed_paths": changed_paths,
        "argv": list(argv),
        "rustc_version_verbose": _capture(["rustc", "-Vv"]),
        "python_version": sys.version,
        "platform_uname": {
            "system": uname.system,
            "node": uname.node,
            "release": uname.release,
            "version": uname.version,
            "machine": uname.machine,
            "processor": uname.processor,
        },
        "cpu_model": _cpu_model(),
        "logical_cpu_count": os.cpu_count(),
        "start_utc": started_utc,
        "end_utc": None,
        "elapsed_seconds": None,
        "status": "running",
        "failed_policy": None,
    }


def _default_run_dir(started: datetime, short_revision: str) -> Path:
    timestamp = started.strftime("%Y%m%dT%H%M%SZ")
    return REPOSITORY_ROOT / "target" / "transaction-bench" / (
        f"{timestamp}-{short_revision}"
    )


def resolve_run_dir(requested: Path) -> Path:
    """Resolve a generated run without allowing it to escape ignored output."""
    output_root = (REPOSITORY_ROOT / "target" / "transaction-bench").resolve()
    candidate = Path(requested).resolve()
    if candidate == output_root or not candidate.is_relative_to(output_root):
        raise ValueError(
            f"output directory must be below repository target/transaction-bench: {candidate}"
        )
    return candidate


def _finish_invocation(
    invocation_path: Path,
    invocation: dict[str, Any],
    started: float,
    status: str,
    failed_policy: str | None = None,
) -> None:
    invocation["end_utc"] = _utc_now()
    invocation["elapsed_seconds"] = time.perf_counter() - started
    invocation["status"] = status
    invocation["failed_policy"] = failed_policy
    _write_json(invocation_path, invocation)


def _mark_invocation_reporting(
    invocation_path: Path, invocation: dict[str, Any]
) -> None:
    invocation["end_utc"] = None
    invocation["elapsed_seconds"] = None
    invocation["status"] = "reporting"
    invocation["failed_policy"] = None
    _write_json(invocation_path, invocation)


def run(args: argparse.Namespace, argv: Sequence[str] | None = None) -> int:
    actual_argv = list(sys.argv if argv is None else argv)
    started_datetime = datetime.now(timezone.utc)
    started_utc = started_datetime.isoformat(timespec="microseconds").replace(
        "+00:00", "Z"
    )
    started = time.perf_counter()
    invocation = _host_metadata(actual_argv, started_utc)
    requested_run_dir = (
        args.output_dir
        if args.output_dir is not None
        else _default_run_dir(started_datetime, invocation["git_short_revision"])
    )
    run_dir = resolve_run_dir(requested_run_dir)
    run_dir.mkdir(parents=True, exist_ok=False)
    invocation["run_dir"] = str(run_dir)
    invocation_path = run_dir / "invocation.json"
    orchestrator_path = run_dir / "orchestrator.jsonl"
    _write_json(invocation_path, invocation)
    _append_jsonl(
        orchestrator_path,
        {
            "record_type": "orchestrator_start",
            "schema_version": SCHEMA_VERSION,
            "started_utc": started_utc,
            "policies": list(args.policies),
            "backends": list(args.backends),
            "workloads": list(args.workloads),
            "workers": list(args.workers),
        },
    )

    completed_policies = 0
    for policy in args.policies:
        try:
            result = run_policy(policy, run_dir, args)
        except Exception as error:
            result = PolicyResult(
                policy=policy,
                returncode=1,
                command=build_command(
                    policy,
                    run_dir / "requests" / f"{policy}.json",
                    run_dir / "raw" / f"{policy}.jsonl",
                ),
                request_path=str(run_dir / "requests" / f"{policy}.json"),
                output_path=str(run_dir / "raw" / f"{policy}.jsonl"),
                started_utc=_utc_now(),
                ended_utc=_utc_now(),
                elapsed_seconds=0.0,
                driver_complete=False,
                error=f"orchestrator error: {error}",
            )
        record = asdict(result)
        record["record_type"] = (
            "policy_complete" if result.succeeded else "policy_failure"
        )
        record["schema_version"] = SCHEMA_VERSION
        _append_jsonl(orchestrator_path, record)
        if not result.succeeded:
            _finish_invocation(
                invocation_path, invocation, started, "failed", failed_policy=policy
            )
            return 1
        completed_policies += 1

    _mark_invocation_reporting(invocation_path, invocation)
    try:
        report_data = reporter._prepare_reports_before_completion(run_dir)
    except Exception as error:
        _append_jsonl(
            orchestrator_path,
            {
                "record_type": "report_failure",
                "schema_version": SCHEMA_VERSION,
                "error": str(error),
                "ended_utc": _utc_now(),
            },
        )
        _finish_invocation(invocation_path, invocation, started, "failed")
        return 1
    _append_jsonl(
        orchestrator_path,
        {
            "record_type": "complete",
            "schema_version": SCHEMA_VERSION,
            "completed_policies": completed_policies,
            "ended_utc": _utc_now(),
        },
    )
    try:
        invocation["status"] = "complete"
        invocation["failed_policy"] = None
        invocation["end_utc"] = _utc_now()
        invocation["elapsed_seconds"] = time.perf_counter() - started
        for _ in range(10):
            displayed_elapsed = f"{invocation['elapsed_seconds']:.3f}"
            reporter._write_terminal_markdown(report_data, invocation)
            invocation["end_utc"] = _utc_now()
            invocation["elapsed_seconds"] = time.perf_counter() - started
            if f"{invocation['elapsed_seconds']:.3f}" == displayed_elapsed:
                break
        else:
            raise RuntimeError("total elapsed display did not stabilize")
    except Exception as error:
        _append_jsonl(
            orchestrator_path,
            {
                "record_type": "report_failure",
                "schema_version": SCHEMA_VERSION,
                "error": str(error),
                "ended_utc": _utc_now(),
            },
        )
        _finish_invocation(invocation_path, invocation, started, "failed")
        return 1
    # Terminal invocation metadata is deliberately the final lifecycle write.
    _write_json(invocation_path, invocation)
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        return run(args, argv=[sys.argv[0], *(argv if argv is not None else sys.argv[1:])])
    except (OSError, RuntimeError, ValueError) as error:
        print(f"transaction-cc-bench: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
