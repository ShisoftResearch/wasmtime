#!/usr/bin/env python3
"""Validate and report transaction concurrency-control benchmark runs."""

from __future__ import annotations

import csv
from dataclasses import dataclass, replace
import json
import math
from pathlib import Path
import struct
import sys
from typing import Any, Iterable, Mapping, Sequence


SCHEMA_VERSION = 2
REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
BACKENDS = ("vmemory", "file-backed")
WORKLOADS = ("read-only", "disjoint-writes", "hot-key", "write-skew")
GC_POLICY_MODES = ("current-state-only", "mvcc-compliant")
TFUNC_CONTROLS = ("read-only", "rmw")
U32_MAX = (1 << 32) - 1
U64_MAX = (1 << 64) - 1
USIZE_MAX = (1 << (8 * struct.calcsize("P"))) - 1
MAX_PHASE_MS = 24 * 60 * 60 * 1000
BASE_POLICY_FEATURES = {
    "lockbased": ("transaction-cc-lockbased",),
    "no-wait": ("transaction-cc-nowait-abort",),
    "optimistic": ("transaction-cc-optimistic-validation",),
    "strict-2pl": ("transaction-cc-strict-2pl",),
    "timestamp": ("transaction-cc-timestamp-ordering",),
    "wait-die": ("transaction-cc-wait-die",),
    "wound-wait": ("transaction-cc-wound-wait",),
}
POLICY_FEATURES = {
    **BASE_POLICY_FEATURES,
    **{
        f"mvcc-{policy}": ("transaction-mvcc", *features)
        for policy, features in BASE_POLICY_FEATURES.items()
    },
}

CellKey = tuple[str, str, str, int, int, int]
ControlKey = tuple[str, str, str, int, int]


@dataclass(frozen=True)
class RunData:
    run_dir: Path
    invocation: Mapping[str, Any]
    policies: tuple[str, ...]
    requests: Mapping[str, Mapping[str, Any]]
    records: tuple[Mapping[str, Any], ...]
    cells: tuple[Mapping[str, Any], ...]
    skipped_cells: tuple[Mapping[str, Any], ...]
    controls: tuple[Mapping[str, Any], ...]
    raw_paths: tuple[Path, ...]
    aggregate_counts: Mapping[str, int]


CSV_COLUMNS = (
    "record_kind",
    "status",
    "policy",
    "visibility_mode",
    "concurrency_control",
    "compiled_features",
    "backend",
    "workload",
    "control",
    "workers",
    "repetition",
    "seed",
    "requested_elapsed_ns",
    "effective_elapsed_ns",
    "attempts_per_second",
    "committed_operations_per_second",
    "p50_ns",
    "p95_ns",
    "p99_ns",
    "attempts",
    "successful_attempts",
    "conflict_aborts",
    "committed_operations",
    "retries",
    "unexpected_errors",
    "fairness_min_max_ratio",
    "fairness_coefficient_of_variation",
    "gc_barriers",
    "gc_total_ns",
    "gc_max_pause_ns",
    "gc_elapsed_share",
    "gc_policy_mode",
    "versions_created",
    "versions_opportunistically_pruned",
    "versions_barrier_pruned",
    "versions_resident_after_final_barrier",
    "schedules_per_second",
    "restore_attempts",
    "restore_successful_attempts",
    "restore_conflict_aborts",
    "restore_committed_operations",
    "restore_retries",
    "restore_unexpected_errors",
    "restore_p50_ns",
    "restore_p95_ns",
    "restore_p99_ns",
    "anomalous_conflict",
    "reason",
)


def _read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read {path}: {error}") from error
    if not isinstance(value, dict):
        raise ValueError(f"{path} must contain a JSON object")
    return value


def _read_jsonl(path: Path) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    try:
        with path.open(encoding="utf-8") as stream:
            for line_number, line in enumerate(stream, start=1):
                if not line.strip():
                    raise ValueError(f"{path}:{line_number}: blank JSONL line")
                value = json.loads(line)
                if not isinstance(value, dict):
                    raise ValueError(f"{path}:{line_number}: record must be an object")
                records.append(value)
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read {path}: {error}") from error
    if not records:
        raise ValueError(f"{path} is empty")
    return records


def _require_schema(record: Mapping[str, Any], context: str) -> None:
    version = record.get("schema_version")
    if (
        isinstance(version, bool)
        or not isinstance(version, int)
        or version != SCHEMA_VERSION
    ):
        raise ValueError(
            f"{context}: schema_version must be {SCHEMA_VERSION}, "
            f"got {version!r}"
        )


def _require_list(value: Any, context: str) -> list[Any]:
    if not isinstance(value, list) or not value:
        raise ValueError(f"{context} must be a nonempty list")
    if len(set(map(_hashable, value))) != len(value):
        raise ValueError(f"{context} must not contain duplicates")
    return value


def _hashable(value: Any) -> Any:
    if isinstance(value, list):
        return tuple(map(_hashable, value))
    if isinstance(value, dict):
        return tuple(sorted((key, _hashable(item)) for key, item in value.items()))
    return value


def _features(policy: str) -> tuple[str, ...]:
    try:
        return POLICY_FEATURES[policy]
    except KeyError as error:
        raise ValueError(f"unknown requested policy {policy!r}") from error


def _policy_identity(policy: str) -> tuple[str, str]:
    _features(policy)
    if policy.startswith("mvcc-"):
        return "mvcc", policy.removeprefix("mvcc-")
    return "single-version", policy


def _validate_features(value: Any, policy: str, context: str) -> None:
    expected = _features(policy)
    if not isinstance(value, list) or tuple(value) != expected:
        raise ValueError(
            f"{context}: compiled transaction features {value!r} do not match "
            f"policy {policy!r}: {list(expected)!r}"
        )


def _validate_identity(value: Mapping[str, Any], policy: str, context: str) -> None:
    expected_visibility, expected_cc = _policy_identity(policy)
    if value.get("visibility_mode") != expected_visibility:
        raise ValueError(
            f"{context}: visibility_mode {value.get('visibility_mode')!r} does not "
            f"match policy {policy!r}: {expected_visibility!r}"
        )
    if value.get("concurrency_control") != expected_cc:
        raise ValueError(
            f"{context}: concurrency_control {value.get('concurrency_control')!r} "
            f"does not match policy {policy!r}: {expected_cc!r}"
        )
    _validate_features(value.get("compiled_features"), policy, context)


def _nonnegative_integer(
    value: Any, context: str, *, maximum: int = U64_MAX
) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise ValueError(f"{context} must be a nonnegative integer")
    if value > maximum:
        raise ValueError(f"{context} exceeds the Rust integer bound {maximum}")
    return value


def _finite_nonnegative(value: Any, context: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError(f"{context} must be a finite nonnegative number")
    converted = float(value)
    if not math.isfinite(converted) or converted < 0:
        raise ValueError(f"{context} must be a finite nonnegative number")
    return converted


def _validate_counts(counts: Any, context: str) -> dict[str, int]:
    if not isinstance(counts, dict):
        raise ValueError(f"{context} must be an object")
    names = (
        "attempts",
        "successful_attempts",
        "conflict_aborts",
        "committed_operations",
        "retries",
        "unexpected_errors",
    )
    parsed = {
        name: _nonnegative_integer(counts.get(name), f"{context}.{name}")
        for name in names
    }
    if parsed["attempts"] != parsed["successful_attempts"] + parsed["conflict_aborts"]:
        raise ValueError(
            f"{context}: attempts must equal successful_attempts plus conflict_aborts"
        )
    if parsed["committed_operations"] != parsed["successful_attempts"]:
        raise ValueError(
            f"{context}: committed_operations must equal successful_attempts"
        )
    if parsed["retries"] != parsed["attempts"] - parsed["committed_operations"]:
        raise ValueError(
            f"{context}: retries must equal attempts minus committed_operations"
        )
    if parsed["unexpected_errors"] != 0:
        raise ValueError(f"{context}: unexpected_errors must be zero")
    return parsed


def _optional_nonnegative_integer(value: Any, context: str) -> None:
    if value is not None:
        _nonnegative_integer(value, context)


def _validate_metrics(metrics: Any, context: str, *, control: bool = False) -> None:
    if not isinstance(metrics, dict):
        raise ValueError(f"{context} must be an object")
    _nonnegative_integer(
        metrics.get("requested_elapsed_ns"), f"{context}.requested_elapsed_ns"
    )
    effective = _nonnegative_integer(
        metrics.get("effective_elapsed_ns"), f"{context}.effective_elapsed_ns"
    )
    if effective == 0:
        raise ValueError(f"{context}.effective_elapsed_ns must be nonzero")
    _finite_nonnegative(metrics.get("attempts_per_second"), f"{context}.attempts_per_second")
    _finite_nonnegative(
        metrics.get("committed_operations_per_second"),
        f"{context}.committed_operations_per_second",
    )
    _validate_counts(metrics.get("counts"), f"{context}.counts")
    for percentile in ("p50_ns", "p95_ns", "p99_ns"):
        _optional_nonnegative_integer(metrics.get(percentile), f"{context}.{percentile}")

    fairness = metrics.get("fairness")
    if not isinstance(fairness, dict):
        raise ValueError(f"{context}.fairness must be an object")
    for name in ("min_max_ratio", "coefficient_of_variation"):
        value = fairness.get(name)
        if value is not None:
            _finite_nonnegative(value, f"{context}.fairness.{name}")

    gc = metrics.get("gc")
    if not isinstance(gc, dict) or not isinstance(gc.get("policy_mode"), str):
        raise ValueError(f"{context}.gc must contain a policy_mode string")
    if gc["policy_mode"] not in GC_POLICY_MODES:
        raise ValueError(
            f"{context}.gc.policy_mode is not a Rust GC policy mode: "
            f"{gc['policy_mode']!r}"
        )
    for name in ("barriers", "total_ns", "max_pause_ns"):
        _nonnegative_integer(gc.get(name), f"{context}.gc.{name}")
    _finite_nonnegative(gc.get("elapsed_share"), f"{context}.gc.elapsed_share")

    versions = metrics.get("versions")
    if not isinstance(versions, dict):
        raise ValueError(f"{context}.versions must be an object")
    for name in (
        "created",
        "opportunistically_pruned",
        "barrier_pruned",
        "resident_after_final_barrier",
    ):
        _nonnegative_integer(versions.get(name), f"{context}.versions.{name}")

    schedules = metrics.get("schedules_per_second")
    if schedules is not None:
        _finite_nonnegative(schedules, f"{context}.schedules_per_second")
    restore = metrics.get("restore")
    if restore is not None:
        if not isinstance(restore, dict):
            raise ValueError(f"{context}.restore must be an object or null")
        _validate_counts(restore.get("counts"), f"{context}.restore.counts")
        for percentile in ("p50_ns", "p95_ns", "p99_ns"):
            _optional_nonnegative_integer(
                restore.get(percentile), f"{context}.restore.{percentile}"
            )
    if control and (schedules is not None or restore is not None):
        raise ValueError(f"{context}: tfunc controls cannot contain write-skew metrics")
    if metrics.get("anomalous_conflict") is not False:
        raise ValueError(f"{context}.anomalous_conflict must be false")


def _cell_key(spec: Any, context: str) -> CellKey:
    if not isinstance(spec, dict):
        raise ValueError(f"{context} must be an object")
    policy = spec.get("policy")
    backend = spec.get("backend")
    workload = spec.get("workload")
    workers = spec.get("workers")
    repetition = spec.get("repetition")
    seed = spec.get("seed")
    if not all(isinstance(value, str) for value in (policy, backend, workload)):
        raise ValueError(f"{context} has invalid string identity fields")
    if any(
        isinstance(value, bool) or not isinstance(value, int)
        for value in (workers, repetition, seed)
    ):
        raise ValueError(f"{context} has invalid integer identity fields")
    if workers <= 0 or repetition < 0 or seed < 0:
        raise ValueError(f"{context} has out-of-range identity fields")
    _nonnegative_integer(workers, f"{context}.workers", maximum=USIZE_MAX)
    _nonnegative_integer(repetition, f"{context}.repetition", maximum=U32_MAX)
    _nonnegative_integer(seed, f"{context}.seed")
    if backend not in BACKENDS:
        raise ValueError(f"{context}.backend is not a Rust backend: {backend!r}")
    if workload not in WORKLOADS:
        raise ValueError(f"{context}.workload is not a Rust workload: {workload!r}")
    _validate_identity(spec, policy, context)
    return policy, backend, workload, workers, repetition, seed


def _load_orchestrator(
    run_dir: Path, *, require_complete: bool
) -> tuple[dict[str, Any], tuple[str, ...]]:
    records = _read_jsonl(run_dir / "orchestrator.jsonl")
    for index, record in enumerate(records):
        _require_schema(record, f"orchestrator record {index}")
        if record.get("record_type") in ("policy_failure", "report_failure"):
            raise ValueError("orchestrator contains a failure record")
    starts = [record for record in records if record.get("record_type") == "orchestrator_start"]
    if len(starts) != 1 or records[0] is not starts[0]:
        raise ValueError("orchestrator must begin with exactly one orchestrator_start")
    policies_value = _require_list(starts[0].get("policies"), "requested policies")
    if not all(isinstance(policy, str) for policy in policies_value):
        raise ValueError("requested policies must be strings")
    policies = tuple(policies_value)
    for policy in policies:
        _features(policy)
        visibility, concurrency_control = _policy_identity(policy)
        if visibility == "mvcc" and concurrency_control not in policies:
            raise ValueError(
                f"MVCC policy {policy!r} requires matching single-version baseline "
                f"{concurrency_control!r}"
            )
    completed = [
        record.get("policy")
        for record in records
        if record.get("record_type") == "policy_complete"
    ]
    if completed != list(policies):
        raise ValueError(
            f"orchestrator policy completion order {completed!r} does not match {list(policies)!r}"
        )
    completions = [
        record for record in records if record.get("record_type") == "complete"
    ]
    if require_complete:
        if len(completions) != 1 or records[-1] is not completions[0]:
            raise ValueError(
                "complete run must end with exactly one final orchestrator complete record"
            )
        completed_policies = _nonnegative_integer(
            completions[0].get("completed_policies"),
            "orchestrator complete.completed_policies",
            maximum=USIZE_MAX,
        )
        if completed_policies != len(policies):
            raise ValueError(
                "orchestrator complete.completed_policies does not match requested policies"
            )
    elif completions:
        raise ValueError("pre-completion reporting cannot contain an orchestrator complete")
    allowed = {"orchestrator_start", "policy_complete", "complete"}
    unknown = [
        record.get("record_type")
        for record in records
        if record.get("record_type") not in allowed
    ]
    if unknown:
        raise ValueError(f"orchestrator contains unexpected records: {unknown!r}")
    return starts[0], policies


def _validate_request(
    request: dict[str, Any], policy: str, start: Mapping[str, Any], context: str
) -> None:
    _require_schema(request, context)
    if request.get("expected_policy") != policy:
        raise ValueError(f"{context}: expected_policy does not match {policy!r}")
    for axis in ("backends", "workloads", "workers"):
        values = _require_list(request.get(axis), f"{context}.{axis}")
        if values != start.get(axis):
            raise ValueError(f"{context}.{axis} does not match orchestration metadata")
    if any(value not in BACKENDS for value in request["backends"]):
        raise ValueError(f"{context}.backends contains a value outside Rust backends")
    if any(value not in WORKLOADS for value in request["workloads"]):
        raise ValueError(f"{context}.workloads contains a value outside Rust workloads")
    for workers in request["workers"]:
        parsed = _nonnegative_integer(
            workers, f"{context}.workers", maximum=USIZE_MAX
        )
        if parsed == 0:
            raise ValueError(f"{context}.workers must be nonzero")
    for name in ("warmup_ms", "measure_ms"):
        value = _nonnegative_integer(request.get(name), f"{context}.{name}")
        if value == 0:
            raise ValueError(f"{context}.{name} must be nonzero")
        if value > MAX_PHASE_MS:
            raise ValueError(f"{context}.{name} exceeds the Rust limit of 24 hours")
    repetitions = _nonnegative_integer(
        request.get("repetitions"), f"{context}.repetitions", maximum=U32_MAX
    )
    if repetitions == 0:
        raise ValueError(f"{context}.repetitions must be nonzero")
    gc_interval = _nonnegative_integer(
        request.get("gc_commit_interval"), f"{context}.gc_commit_interval"
    )
    if gc_interval == 0:
        raise ValueError(f"{context}.gc_commit_interval must be nonzero")
    _nonnegative_integer(request.get("seed"), f"{context}.seed")
    if not isinstance(request.get("include_tfunc_control"), bool):
        raise ValueError(f"{context}.include_tfunc_control must be boolean")


def _expected_cell_keys(policy: str, request: Mapping[str, Any]) -> set[CellKey]:
    return {
        (policy, backend, workload, workers, repetition, request["seed"])
        for backend in request["backends"]
        for workload in request["workloads"]
        for workers in request["workers"]
        for repetition in range(request["repetitions"])
    }


def _load_run(run_dir: Path, *, lifecycle: str) -> RunData:
    run_dir = Path(run_dir)
    invocation = _read_json(run_dir / "invocation.json")
    _require_schema(invocation, "invocation")
    if lifecycle not in ("complete", "reporting"):
        raise AssertionError(f"unknown report lifecycle {lifecycle!r}")
    if invocation.get("status") != lifecycle:
        raise ValueError(f"invocation status must be {lifecycle} for this report phase")
    if lifecycle == "complete":
        elapsed = invocation.get("elapsed_seconds")
        _finite_nonnegative(elapsed, "invocation.elapsed_seconds")
        if not isinstance(invocation.get("end_utc"), str):
            raise ValueError("complete invocation must contain end_utc")
    elif invocation.get("end_utc") is not None or invocation.get("elapsed_seconds") is not None:
        raise ValueError("reporting invocation must remain nonterminal")

    start, policies = _load_orchestrator(
        run_dir, require_complete=lifecycle == "complete"
    )
    requests: dict[str, Mapping[str, Any]] = {}
    all_records: list[Mapping[str, Any]] = []
    cells: list[Mapping[str, Any]] = []
    skipped: list[Mapping[str, Any]] = []
    controls: list[Mapping[str, Any]] = []
    raw_paths: list[Path] = []
    seen_cells: set[CellKey] = set()
    seen_controls: set[ControlKey] = set()
    profile_identity: tuple[Any, ...] | None = None

    raw_dir = run_dir / "raw"
    actual_raw = {path.name for path in raw_dir.glob("*.jsonl")}
    expected_raw = {f"{policy}.jsonl" for policy in policies}
    if actual_raw != expected_raw:
        raise ValueError(
            f"raw policy files {sorted(actual_raw)!r} do not match requested "
            f"files {sorted(expected_raw)!r}"
        )

    for policy in policies:
        request = _read_json(run_dir / "requests" / f"{policy}.json")
        _validate_request(request, policy, start, f"request {policy}")
        current_profile = tuple(
            _hashable(request[name])
            for name in (
                "warmup_ms",
                "measure_ms",
                "repetitions",
                "backends",
                "workloads",
                "workers",
                "seed",
                "gc_commit_interval",
                "include_tfunc_control",
            )
        )
        if profile_identity is None:
            profile_identity = current_profile
        elif current_profile != profile_identity:
            raise ValueError(
                f"request {policy}: benchmark profile differs from earlier policies"
            )
        requests[policy] = request
        path = raw_dir / f"{policy}.jsonl"
        raw_paths.append(path)
        records = _read_jsonl(path)
        all_records.extend(records)

        for index, record in enumerate(records):
            _require_schema(record, f"{path}:{index + 1}")
        if records[0].get("record_type") != "driver":
            raise ValueError(f"{path} must begin with a driver identity")
        if records[-1].get("record_type") != "complete":
            raise ValueError(f"{path} is missing its complete footer")
        drivers = [record for record in records if record.get("record_type") == "driver"]
        footers = [record for record in records if record.get("record_type") == "complete"]
        if len(drivers) != 1:
            raise ValueError(f"{path} must contain exactly one driver identity")
        if len(footers) != 1:
            raise ValueError(f"{path} must contain exactly one complete footer")
        if any(record.get("record_type") == "failure" for record in records):
            raise ValueError(f"{path} contains a failure record")

        driver = drivers[0]
        if driver.get("compiled_policy") != policy:
            raise ValueError(f"{path}: compiled policy does not match requested policy")
        _validate_identity(driver, policy, f"{path} driver")
        available = _nonnegative_integer(
            driver.get("available_parallelism"),
            f"{path} available_parallelism",
            maximum=USIZE_MAX,
        )
        if available == 0:
            raise ValueError(f"{path}: available_parallelism must be nonzero")
        gc_policy_mode = driver.get("gc_policy_mode")
        if gc_policy_mode not in GC_POLICY_MODES:
            raise ValueError(
                f"{path}: driver GC policy mode is outside the Rust domain: "
                f"{gc_policy_mode!r}"
            )
        expected_gc_policy_mode = (
            "mvcc-compliant"
            if driver["visibility_mode"] == "mvcc"
            else "current-state-only"
        )
        if gc_policy_mode != expected_gc_policy_mode:
            raise ValueError(
                f"{path}: driver GC policy mode {gc_policy_mode!r} does not match "
                f"visibility mode {driver['visibility_mode']!r}"
            )

        policy_cells: list[Mapping[str, Any]] = []
        policy_skipped: list[Mapping[str, Any]] = []
        policy_controls: list[Mapping[str, Any]] = []
        for index, record in enumerate(records[1:-1], start=2):
            record_type = record.get("record_type")
            context = f"{path}:{index}"
            if record_type in ("cell", "skipped_cell"):
                key = _cell_key(record.get("spec"), f"{context}.spec")
                if key[0] != policy:
                    raise ValueError(f"{context}: cell policy does not match raw stream")
                if key in seen_cells:
                    raise ValueError(f"duplicate cell identity {key!r}")
                seen_cells.add(key)
                if record_type == "cell":
                    if key[3] > available:
                        raise ValueError(f"{context}: unsupported worker cell was not skipped")
                    _validate_metrics(record.get("metrics"), f"{context}.metrics")
                    if record["metrics"]["gc"]["policy_mode"] != gc_policy_mode:
                        raise ValueError(f"{context}: cell GC policy differs from driver")
                    policy_cells.append(record)
                    cells.append(record)
                else:
                    if key[3] <= available:
                        raise ValueError(f"{context}: supported worker cell was skipped")
                    if not isinstance(record.get("reason"), str) or not record["reason"]:
                        raise ValueError(f"{context}: skipped cell must have a reason")
                    policy_skipped.append(record)
                    skipped.append(record)
            elif record_type == "tfunc_control":
                if record.get("policy") != policy:
                    raise ValueError(f"{context}: control policy does not match raw stream")
                _validate_identity(record, policy, context)
                backend = record.get("backend")
                control = record.get("control")
                repetition = record.get("repetition")
                seed = record.get("seed")
                if not isinstance(backend, str) or not isinstance(control, str):
                    raise ValueError(f"{context}: invalid control identity")
                _nonnegative_integer(
                    repetition, f"{context}.repetition", maximum=U32_MAX
                )
                _nonnegative_integer(seed, f"{context}.seed")
                if (
                    backend not in request["backends"]
                    or control not in TFUNC_CONTROLS
                    or repetition >= request["repetitions"]
                    or seed != request["seed"]
                    or not request["include_tfunc_control"]
                ):
                    raise ValueError(f"{context}: control identity is outside the request")
                key: ControlKey = policy, backend, control, repetition, seed
                if key in seen_controls:
                    raise ValueError(f"duplicate tfunc control identity {key!r}")
                seen_controls.add(key)
                _validate_metrics(record.get("metrics"), f"{context}.metrics", control=True)
                if record["metrics"]["gc"]["policy_mode"] != gc_policy_mode:
                    raise ValueError(f"{context}: control GC policy differs from driver")
                policy_controls.append(record)
                controls.append(record)
            else:
                raise ValueError(f"{context}: unexpected record_type {record_type!r}")

        expected_keys = _expected_cell_keys(policy, request)
        actual_keys = {
            _cell_key(record["spec"], f"{path} matrix")
            for record in (*policy_cells, *policy_skipped)
        }
        missing = expected_keys - actual_keys
        extra = actual_keys - expected_keys
        if missing:
            raise ValueError(f"{path}: missing matrix cell {sorted(missing)!r}")
        if extra:
            raise ValueError(f"{path}: unexpected matrix cell {sorted(extra)!r}")
        actual_control_keys = {
            (
                record["policy"],
                record["backend"],
                record["control"],
                record["repetition"],
                record["seed"],
            )
            for record in policy_controls
        }
        expected_control_keys = (
            {
                (policy, backend, control, repetition, request["seed"])
                for backend in request["backends"]
                for control in TFUNC_CONTROLS
                for repetition in range(request["repetitions"])
            }
            if request["include_tfunc_control"]
            else set()
        )
        missing_controls = expected_control_keys - actual_control_keys
        extra_controls = actual_control_keys - expected_control_keys
        if missing_controls:
            raise ValueError(f"{path}: missing tfunc control {sorted(missing_controls)!r}")
        if extra_controls:
            raise ValueError(f"{path}: unexpected tfunc control {sorted(extra_controls)!r}")
        footer = footers[0]
        if footer.get("policy") != policy:
            raise ValueError(f"{path}: complete footer policy mismatch")
        expected_counts = (
            len(policy_cells),
            len(policy_skipped),
            len(policy_controls),
        )
        actual_counts = tuple(
            _nonnegative_integer(
                footer.get(name), f"{path} footer {name}", maximum=USIZE_MAX
            )
            for name in ("completed_cells", "skipped_cells", "tfunc_controls")
        )
        if actual_counts != expected_counts:
            raise ValueError(
                f"{path}: complete footer counts {actual_counts!r} do not match "
                f"records {expected_counts!r}"
            )

    aggregate = {"attempts": 0, "committed": 0, "aborts": 0, "retries": 0}
    for record in (*cells, *controls):
        counts = record["metrics"]["counts"]
        aggregate["attempts"] += counts["attempts"]
        aggregate["committed"] += counts["committed_operations"]
        aggregate["aborts"] += counts["conflict_aborts"]
        aggregate["retries"] += counts["retries"]

    return RunData(
        run_dir=run_dir,
        invocation=invocation,
        policies=policies,
        requests=requests,
        records=tuple(all_records),
        cells=tuple(cells),
        skipped_cells=tuple(skipped),
        controls=tuple(controls),
        raw_paths=tuple(raw_paths),
        aggregate_counts=aggregate,
    )


def load_complete_run(run_dir: Path) -> RunData:
    """Load a terminal run with exactly one matching lifecycle footer."""
    return _load_run(Path(run_dir), lifecycle="complete")


def _empty_row() -> dict[str, Any]:
    return {column: "" for column in CSV_COLUMNS}


def _metrics_row(metrics: Mapping[str, Any]) -> dict[str, Any]:
    counts = metrics["counts"]
    fairness = metrics["fairness"]
    gc = metrics["gc"]
    versions = metrics["versions"]
    row = {
        "requested_elapsed_ns": metrics["requested_elapsed_ns"],
        "effective_elapsed_ns": metrics["effective_elapsed_ns"],
        "attempts_per_second": metrics["attempts_per_second"],
        "committed_operations_per_second": metrics["committed_operations_per_second"],
        "p50_ns": metrics["p50_ns"],
        "p95_ns": metrics["p95_ns"],
        "p99_ns": metrics["p99_ns"],
        "attempts": counts["attempts"],
        "successful_attempts": counts["successful_attempts"],
        "conflict_aborts": counts["conflict_aborts"],
        "committed_operations": counts["committed_operations"],
        "retries": counts["retries"],
        "unexpected_errors": counts["unexpected_errors"],
        "fairness_min_max_ratio": fairness["min_max_ratio"],
        "fairness_coefficient_of_variation": fairness["coefficient_of_variation"],
        "gc_barriers": gc["barriers"],
        "gc_total_ns": gc["total_ns"],
        "gc_max_pause_ns": gc["max_pause_ns"],
        "gc_elapsed_share": gc["elapsed_share"],
        "gc_policy_mode": gc["policy_mode"],
        "versions_created": versions["created"],
        "versions_opportunistically_pruned": versions["opportunistically_pruned"],
        "versions_barrier_pruned": versions["barrier_pruned"],
        "versions_resident_after_final_barrier": versions[
            "resident_after_final_barrier"
        ],
        "schedules_per_second": metrics["schedules_per_second"],
        "anomalous_conflict": metrics["anomalous_conflict"],
    }
    restore = metrics["restore"]
    if restore is not None:
        for name, value in restore["counts"].items():
            row[f"restore_{name}"] = value
        for percentile in ("p50_ns", "p95_ns", "p99_ns"):
            row[f"restore_{percentile}"] = restore[percentile]
    return {key: "" if value is None else value for key, value in row.items()}


def _csv_rows(data: RunData) -> Iterable[dict[str, Any]]:
    for record in data.records:
        record_type = record["record_type"]
        if record_type in ("cell", "skipped_cell"):
            spec = record["spec"]
            row = _empty_row()
            row.update(
                {
                    "record_kind": "cell",
                    "status": "completed" if record_type == "cell" else "skipped",
                    "policy": spec["policy"],
                    "visibility_mode": spec["visibility_mode"],
                    "concurrency_control": spec["concurrency_control"],
                    "compiled_features": ",".join(spec["compiled_features"]),
                    "backend": spec["backend"],
                    "workload": spec["workload"],
                    "workers": spec["workers"],
                    "repetition": spec["repetition"],
                    "seed": spec["seed"],
                }
            )
            if record_type == "cell":
                row.update(_metrics_row(record["metrics"]))
            else:
                row["reason"] = record["reason"]
            yield row
        elif record_type == "tfunc_control":
            row = _empty_row()
            row.update(
                {
                    "record_kind": "tfunc_control",
                    "status": "control",
                    "policy": record["policy"],
                    "visibility_mode": record["visibility_mode"],
                    "concurrency_control": record["concurrency_control"],
                    "compiled_features": ",".join(record["compiled_features"]),
                    "backend": record["backend"],
                    "control": record["control"],
                    "workers": 1,
                    "repetition": record["repetition"],
                    "seed": record["seed"],
                }
            )
            row.update(_metrics_row(record["metrics"]))
            yield row


def _atomic_text(path: Path, contents: str) -> None:
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_text(contents, encoding="utf-8")
    temporary.replace(path)


def write_csv(data: RunData, path: Path) -> None:
    """Write stable, analysis-friendly rows for cells, skips, and controls."""
    path = Path(path)
    temporary = path.with_name(f".{path.name}.tmp")
    with temporary.open("w", newline="", encoding="utf-8") as stream:
        writer = csv.DictWriter(stream, fieldnames=CSV_COLUMNS, lineterminator="\n")
        writer.writeheader()
        writer.writerows(_csv_rows(data))
    temporary.replace(path)


def _format_rate(value: Any) -> str:
    return f"{float(value):,.1f}"


def _format_optional(value: Any, suffix: str = "") -> str:
    return "n/a" if value is None else f"{value}{suffix}"


def _ratio(numerator: float, denominator: float) -> str:
    if denominator == 0:
        return "n/a"
    ratio = numerator / denominator
    return f"{ratio:.3f}x ({(ratio - 1.0) * 100:+.1f}%)"


def _cell_identity(record: Mapping[str, Any], *, without: str | None = None) -> tuple[Any, ...]:
    spec = record["spec"]
    fields = ("policy", "backend", "workload", "workers", "repetition", "seed")
    return tuple(spec[field] for field in fields if field != without)


def _comparison_table(
    title: str,
    headers: Sequence[str],
    rows: Iterable[Sequence[Any]],
) -> list[str]:
    result = [f"## {title}", "", "| " + " | ".join(headers) + " |"]
    result.append("| " + " | ".join("---" for _ in headers) + " |")
    materialized = list(rows)
    if materialized:
        result.extend("| " + " | ".join(map(str, row)) + " |" for row in materialized)
    else:
        result.append("| n/a " + " | n/a" * (len(headers) - 1) + " |")
    result.append("")
    return result


def write_markdown(data: RunData, path: Path) -> None:
    """Write a compact human-readable report derived only from validated JSONL."""
    invocation = data.invocation
    requests = data.requests
    first_request = requests[data.policies[0]]
    lines = [
        "# Transaction concurrency-control benchmark",
        "",
        "## Host and revision",
        "",
        f"- Revision: `{invocation['git_revision']}`",
        f"- Host: `{invocation['platform_uname']['node']}` ({invocation['cpu_model']}, "
        f"{invocation['logical_cpu_count']} logical CPUs)",
        f"- Dirty state: **{'dirty' if invocation['dirty'] else 'clean'}**",
        f"- Profile: warmup {first_request['warmup_ms']} ms, measurement "
        f"{first_request['measure_ms']} ms, {first_request['repetitions']} repetition(s)",
        f"- Policies: {', '.join(data.policies)}",
        "",
    ]

    core_rows = []
    for record in data.cells:
        spec = record["spec"]
        metrics = record["metrics"]
        counts = metrics["counts"]
        abort_rate = counts["conflict_aborts"] / counts["attempts"] if counts["attempts"] else 0.0
        retry_rate = (
            counts["retries"] / counts["committed_operations"]
            if counts["committed_operations"]
            else 0.0
        )
        base = next(
            (
                candidate
                for candidate in data.cells
                if candidate["spec"]["policy"] == spec["policy"]
                and candidate["spec"]["backend"] == spec["backend"]
                and candidate["spec"]["workload"] == spec["workload"]
                and candidate["spec"]["workers"] == 1
                and candidate["spec"]["repetition"] == spec["repetition"]
                and candidate["spec"]["seed"] == spec["seed"]
            ),
            None,
        )
        scaling = (
            _ratio(
                metrics["committed_operations_per_second"],
                base["metrics"]["committed_operations_per_second"],
            )
            if base is not None
            else "n/a"
        )
        core_rows.append(
            (
                spec["backend"],
                spec["workload"],
                spec["policy"],
                spec["workers"],
                spec["repetition"],
                spec["seed"],
                _format_rate(metrics["committed_operations_per_second"]),
                scaling,
                f"{abort_rate:.2%}",
                f"{retry_rate:.3f}",
                _format_optional(metrics["p99_ns"]),
                _format_optional(metrics["fairness"]["min_max_ratio"]),
                f"{metrics['gc']['elapsed_share']:.3%}",
            )
        )
    lines.extend(
        _comparison_table(
            "Scaling",
            (
                "backend",
                "workload",
                "policy",
                "workers",
                "repetition",
                "seed",
                "commits/s",
                "vs 1 worker",
                "abort rate",
                "retries/commit",
                "p99 ns",
                "fairness min/max",
                "GC share",
            ),
            core_rows,
        )
    )

    single_version = {
        (
            record["spec"]["concurrency_control"],
            *_cell_identity(record, without="policy"),
        ): record
        for record in data.cells
        if record["spec"]["visibility_mode"] == "single-version"
    }
    mvcc_rows = []
    for record in data.cells:
        if record["spec"]["visibility_mode"] != "mvcc":
            continue
        spec = record["spec"]
        baseline = single_version.get(
            (
                spec["concurrency_control"],
                *_cell_identity(record, without="policy"),
            )
        )
        if baseline is None:
            continue
        mvcc_rows.append(
            (
                spec["concurrency_control"],
                spec["backend"],
                spec["workload"],
                spec["workers"],
                spec["repetition"],
                spec["seed"],
                _ratio(
                    record["metrics"]["committed_operations_per_second"],
                    baseline["metrics"]["committed_operations_per_second"],
                ),
            )
        )
    lines.extend(
        _comparison_table(
            "MVCC versus matching single-version CC",
            (
                "concurrency control",
                "backend",
                "workload",
                "workers",
                "repetition",
                "seed",
                "MVCC/single-version commits/s",
            ),
            mvcc_rows,
        )
    )

    vmemory = {
        _cell_identity(record, without="backend"): record for record in data.cells
        if record["spec"]["backend"] == "vmemory"
    }
    backend_rows = []
    for record in data.cells:
        if record["spec"]["backend"] != "file-backed":
            continue
        baseline = vmemory.get(_cell_identity(record, without="backend"))
        if baseline is None:
            continue
        spec = record["spec"]
        backend_rows.append(
            (
                spec["policy"],
                spec["workload"],
                spec["workers"],
                spec["repetition"],
                spec["seed"],
                _ratio(
                    record["metrics"]["committed_operations_per_second"],
                    baseline["metrics"]["committed_operations_per_second"],
                ),
            )
        )
    lines.extend(
        _comparison_table(
            "Backend delta",
            (
                "policy",
                "workload",
                "workers",
                "repetition",
                "seed",
                "file-backed/vmemory commits/s",
            ),
            backend_rows,
        )
    )

    abort_rows = []
    for record in data.cells:
        spec = record["spec"]
        counts = record["metrics"]["counts"]
        abort_rows.append(
            (
                spec["policy"],
                spec["backend"],
                spec["workload"],
                spec["workers"],
                spec["repetition"],
                spec["seed"],
                counts["attempts"],
                counts["conflict_aborts"],
                counts["retries"],
            )
        )
    lines.extend(
        _comparison_table(
            "Abort and retry",
            (
                "policy",
                "backend",
                "workload",
                "workers",
                "repetition",
                "seed",
                "attempts",
                "aborts",
                "retries",
            ),
            abort_rows,
        )
    )

    gc_rows = []
    for record in data.cells:
        spec = record["spec"]
        gc = record["metrics"]["gc"]
        versions = record["metrics"]["versions"]
        gc_rows.append(
            (
                spec["policy"],
                spec["backend"],
                spec["workload"],
                spec["workers"],
                spec["repetition"],
                spec["seed"],
                gc["policy_mode"],
                gc["barriers"],
                f"{gc['elapsed_share']:.3%}",
                versions["created"],
                versions["opportunistically_pruned"],
                versions["barrier_pruned"],
                versions["resident_after_final_barrier"],
            )
        )
    lines.extend(
        _comparison_table(
            "GC and versions",
            (
                "policy",
                "backend",
                "workload",
                "workers",
                "repetition",
                "seed",
                "mode",
                "barriers",
                "GC share",
                "created",
                "opportunistic prune",
                "barrier prune",
                "resident",
            ),
            gc_rows,
        )
    )

    control_rows = []
    for control in data.controls:
        matching_workload = "read-only" if control["control"] == "read-only" else "hot-key"
        baseline = next(
            (
                record for record in data.cells
                if record["spec"]["policy"] == control["policy"]
                and record["spec"]["backend"] == control["backend"]
                and record["spec"]["workload"] == matching_workload
                and record["spec"]["workers"] == 1
                and record["spec"]["repetition"] == control["repetition"]
                and record["spec"]["seed"] == control["seed"]
            ),
            None,
        )
        ratio = "n/a" if baseline is None else _ratio(
            control["metrics"]["committed_operations_per_second"],
            baseline["metrics"]["committed_operations_per_second"],
        )
        control_rows.append(
            (
                control["policy"],
                control["backend"],
                control["control"],
                control["repetition"],
                control["seed"],
                _format_rate(control["metrics"]["committed_operations_per_second"]),
                ratio,
            )
        )
    lines.extend(
        _comparison_table(
            "tfunc controls",
            (
                "policy",
                "backend",
                "control",
                "repetition",
                "seed",
                "commits/s",
                "tfunc/core",
            ),
            control_rows,
        )
    )

    skip_rows = [
        (
            record["spec"]["policy"],
            record["spec"]["backend"],
            record["spec"]["workload"],
            record["spec"]["workers"],
            record["spec"]["repetition"],
            record["spec"]["seed"],
            record["reason"],
        )
        for record in data.skipped_cells
    ]
    lines.extend(
        _comparison_table(
            "Skipped cells",
            (
                "policy",
                "backend",
                "workload",
                "workers",
                "repetition",
                "seed",
                "reason",
            ),
            skip_rows,
        )
    )

    unavailable = [
        record for record in (*data.cells, *data.controls)
        if record["metrics"]["p99_ns"] is None
    ]
    lines.extend(
        [
            "## Low-sample percentiles",
            "",
            (
                f"{len(unavailable)} metric row(s) report p99 as unavailable because the "
                "histogram did not contain enough samples."
            ),
            "",
            "## Aggregate counts",
            "",
            "| counter | value |",
            "| --- | ---: |",
            f"| attempts | {data.aggregate_counts['attempts']} |",
            f"| committed operations | {data.aggregate_counts['committed']} |",
            f"| conflict aborts | {data.aggregate_counts['aborts']} |",
            f"| retries | {data.aggregate_counts['retries']} |",
            "",
            f"Actual total elapsed time: **{float(invocation['elapsed_seconds']):.3f} seconds**.",
            "",
        ]
    )
    _atomic_text(Path(path), "\n".join(lines))


def _write_merged_jsonl(data: RunData, path: Path) -> None:
    temporary = Path(path).with_name(f".{Path(path).name}.tmp")
    with temporary.open("wb") as output:
        for raw_path in data.raw_paths:
            contents = raw_path.read_bytes()
            output.write(contents)
            if contents and not contents.endswith(b"\n"):
                output.write(b"\n")
    temporary.replace(path)


def generate_reports(run_dir: Path) -> RunData:
    """Validate a run and atomically create all three derived artifacts."""
    data = load_complete_run(Path(run_dir))
    _write_merged_jsonl(data, data.run_dir / "results.jsonl")
    write_csv(data, data.run_dir / "results.csv")
    write_markdown(data, data.run_dir / "summary.md")
    return data


def _prepare_reports_before_completion(run_dir: Path) -> RunData:
    """Validate the nonterminal run and create footer-independent artifacts."""
    data = _load_run(Path(run_dir), lifecycle="reporting")
    _write_merged_jsonl(data, data.run_dir / "results.jsonl")
    write_csv(data, data.run_dir / "results.csv")
    return data


def _write_terminal_markdown(
    data: RunData, terminal_invocation: Mapping[str, Any]
) -> None:
    """Finish a prepared report using terminal metadata held in memory."""
    write_markdown(
        replace(data, invocation=dict(terminal_invocation)),
        data.run_dir / "summary.md",
    )


def _contained_cli_run_dir(run_dir: Path) -> Path:
    output_root = (REPOSITORY_ROOT / "target" / "transaction-bench").resolve()
    candidate = Path(run_dir).resolve()
    if candidate == output_root or not candidate.is_relative_to(output_root):
        raise ValueError(
            f"run directory must be below repository target/transaction-bench: {candidate}"
        )
    return candidate


def main(argv: Sequence[str] | None = None) -> int:
    arguments = list(sys.argv[1:] if argv is None else argv)
    if len(arguments) != 1:
        print("usage: transaction_cc_bench_report.py RUN_DIR", file=sys.stderr)
        return 2
    try:
        generate_reports(_contained_cli_run_dir(Path(arguments[0])))
    except (OSError, ValueError) as error:
        print(f"transaction-cc-bench-report: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
