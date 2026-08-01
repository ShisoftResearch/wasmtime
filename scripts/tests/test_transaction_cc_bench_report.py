import csv
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).parents[1] / "transaction_cc_bench_report.py"
SPEC = importlib.util.spec_from_file_location("transaction_cc_bench_report", SCRIPT)
report = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = report
SPEC.loader.exec_module(report)


FEATURES = {
    "optimistic": ["transaction-cc-optimistic-validation"],
    "mvcc-optimistic": [
        "transaction-mvcc",
        "transaction-cc-optimistic-validation",
    ],
}


def metrics(multiplier=1):
    commits = 100 * multiplier
    aborts = 10 * multiplier
    return {
        "requested_elapsed_ns": 1_000_000,
        "effective_elapsed_ns": 1_100_000,
        "attempts_per_second": 100_000.0 * multiplier,
        "committed_operations_per_second": 90_000.0 * multiplier,
        "counts": {
            "attempts": commits + aborts,
            "successful_attempts": commits,
            "conflict_aborts": aborts,
            "committed_operations": commits,
            "retries": aborts,
            "unexpected_errors": 0,
        },
        "p50_ns": 100,
        "p95_ns": 200,
        "p99_ns": 300,
        "fairness": {
            "min_max_ratio": 0.9,
            "coefficient_of_variation": 0.1,
        },
        "gc": {
            "barriers": 2,
            "total_ns": 1_000,
            "max_pause_ns": 700,
            "elapsed_share": 0.001,
            "policy_mode": "mvcc-compliant" if multiplier == 2 else "current-state-only",
        },
        "versions": {
            "created": 20 if multiplier == 2 else 0,
            "opportunistically_pruned": 4 if multiplier == 2 else 0,
            "barrier_pruned": 15 if multiplier == 2 else 0,
            "resident_after_final_barrier": 1 if multiplier == 2 else 0,
        },
        "schedules_per_second": None,
        "restore": None,
        "anomalous_conflict": False,
    }


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value) + "\n", encoding="utf-8")


def write_jsonl(path, records):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        "".join(json.dumps(record) + "\n" for record in records),
        encoding="utf-8",
    )


def make_run(directory):
    run_dir = Path(directory)
    policies = ["optimistic", "mvcc-optimistic"]
    invocation = {
        "schema_version": 1,
        "git_revision": "0123456789abcdef" * 2 + "01234567",
        "git_short_revision": "0123456789ab",
        "dirty": True,
        "changed_paths": ["crates/wasmtime/src/runtime/transaction/example.rs"],
        "argv": ["scripts/transaction-cc-bench.py", "--profile", "quick"],
        "rustc_version_verbose": "rustc fixture",
        "python_version": "Python fixture",
        "platform_uname": {
            "system": "Linux",
            "node": "fixture-host",
            "release": "fixture-release",
            "version": "fixture-version",
            "machine": "x86_64",
            "processor": "fixture-cpu",
        },
        "cpu_model": "Fixture CPU",
        "logical_cpu_count": 2,
        "start_utc": "2026-08-01T00:00:00Z",
        "end_utc": "2026-08-01T00:00:12Z",
        "elapsed_seconds": 12.5,
        "status": "complete",
        "failed_policy": None,
        "run_dir": str(run_dir),
    }
    write_json(run_dir / "invocation.json", invocation)
    write_jsonl(
        run_dir / "orchestrator.jsonl",
        [
            {
                "record_type": "orchestrator_start",
                "schema_version": 1,
                "started_utc": invocation["start_utc"],
                "policies": policies,
                "backends": ["vmemory", "file-backed"],
                "workloads": ["hot-key"],
                "workers": [1, 2, 4],
            },
            *[
                {
                    "record_type": "policy_complete",
                    "schema_version": 1,
                    "policy": policy,
                }
                for policy in policies
            ],
            {
                "record_type": "complete",
                "schema_version": 1,
                "completed_policies": 2,
                "ended_utc": invocation["end_utc"],
            },
        ],
    )

    for policy_index, policy in enumerate(policies, start=1):
        policy_mode = (
            "mvcc-compliant" if policy == "mvcc-optimistic" else "current-state-only"
        )
        request = {
            "schema_version": 1,
            "expected_policy": policy,
            "warmup_ms": 100,
            "measure_ms": 500,
            "repetitions": 1,
            "backends": ["vmemory", "file-backed"],
            "workloads": ["hot-key"],
            "workers": [1, 2, 4],
            "seed": 42,
            "gc_commit_interval": 4096,
            "include_tfunc_control": True,
        }
        write_json(run_dir / "requests" / f"{policy}.json", request)
        records = [
            {
                "record_type": "driver",
                "schema_version": 1,
                "compiled_policy": policy,
                "compiled_features": FEATURES[policy],
                "available_parallelism": 2,
                "gc_policy_mode": policy_mode,
            }
        ]
        for backend in request["backends"]:
            for workers in request["workers"]:
                spec = {
                    "policy": policy,
                    "compiled_features": FEATURES[policy],
                    "backend": backend,
                    "workload": "hot-key",
                    "workers": workers,
                    "repetition": 0,
                    "seed": 42,
                }
                if workers > 2:
                    records.append(
                        {
                            "record_type": "skipped_cell",
                            "schema_version": 1,
                            "spec": spec,
                            "reason": "worker count 4 exceeds available parallelism 2",
                        }
                    )
                else:
                    cell_metrics = metrics(policy_index * workers)
                    cell_metrics["gc"]["policy_mode"] = policy_mode
                    records.append(
                        {
                            "record_type": "cell",
                            "schema_version": 1,
                            "spec": spec,
                            "metrics": cell_metrics,
                        }
                    )
        for backend in request["backends"]:
            for control in ("read-only", "rmw"):
                control_metrics = metrics(policy_index)
                control_metrics["gc"]["policy_mode"] = policy_mode
                records.append(
                    {
                        "record_type": "tfunc_control",
                        "schema_version": 1,
                        "policy": policy,
                        "compiled_features": FEATURES[policy],
                        "backend": backend,
                        "control": control,
                        "repetition": 0,
                        "seed": 42,
                        "metrics": control_metrics,
                    }
                )
        records.append(
            {
                "record_type": "complete",
                "schema_version": 1,
                "policy": policy,
                "completed_cells": 4,
                "skipped_cells": 2,
                "tfunc_controls": 4,
            }
        )
        write_jsonl(run_dir / "raw" / f"{policy}.jsonl", records)
    return run_dir


class TransactionCcBenchReportTests(unittest.TestCase):
    def test_duplicate_cell_identity_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = make_run(directory)
            raw = run_dir / "raw" / "optimistic.jsonl"
            records = [json.loads(line) for line in raw.read_text().splitlines()]
            records.insert(2, records[1])
            records[-1]["completed_cells"] += 1
            write_jsonl(raw, records)

            with self.assertRaisesRegex(ValueError, "duplicate cell identity"):
                report.load_complete_run(run_dir)

    def test_missing_complete_footer_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = make_run(directory)
            raw = run_dir / "raw" / "optimistic.jsonl"
            records = [json.loads(line) for line in raw.read_text().splitlines()][:-1]
            write_jsonl(raw, records)

            with self.assertRaisesRegex(ValueError, "complete footer"):
                report.load_complete_run(run_dir)

    def test_public_loader_requires_one_final_orchestrator_complete(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = make_run(directory)
            path = run_dir / "orchestrator.jsonl"
            records = [json.loads(line) for line in path.read_text().splitlines()]
            write_jsonl(path, records[:-1])

            with self.assertRaisesRegex(ValueError, "final orchestrator complete"):
                report.load_complete_run(run_dir)

            records[-1]["completed_policies"] = 1
            write_jsonl(path, records)
            with self.assertRaisesRegex(ValueError, "completed_policies"):
                report.load_complete_run(run_dir)

    def test_mixed_schema_versions_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = make_run(directory)
            raw = run_dir / "raw" / "mvcc-optimistic.jsonl"
            records = [json.loads(line) for line in raw.read_text().splitlines()]
            records[2]["schema_version"] = 2
            write_jsonl(raw, records)

            with self.assertRaisesRegex(ValueError, "schema_version"):
                report.load_complete_run(run_dir)

    def test_missing_matrix_cell_is_rejected_even_when_footer_counts_match(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = make_run(directory)
            raw = run_dir / "raw" / "optimistic.jsonl"
            records = [json.loads(line) for line in raw.read_text().splitlines()]
            records.pop(1)
            records[-1]["completed_cells"] -= 1
            write_jsonl(raw, records)

            with self.assertRaisesRegex(ValueError, "missing matrix cell"):
                report.load_complete_run(run_dir)

    def test_tfunc_control_outside_requested_identity_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = make_run(directory)
            raw = run_dir / "raw" / "optimistic.jsonl"
            records = [json.loads(line) for line in raw.read_text().splitlines()]
            control = next(
                record for record in records if record["record_type"] == "tfunc_control"
            )
            control["seed"] = 99
            write_jsonl(raw, records)

            with self.assertRaisesRegex(ValueError, "control identity"):
                report.load_complete_run(run_dir)

    def test_missing_requested_tfunc_control_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = make_run(directory)
            raw = run_dir / "raw" / "optimistic.jsonl"
            records = [json.loads(line) for line in raw.read_text().splitlines()]
            missing_index = next(
                index
                for index, record in enumerate(records)
                if record.get("record_type") == "tfunc_control"
                and record["backend"] == "file-backed"
                and record["control"] == "read-only"
            )
            records.pop(missing_index)
            records[-1]["tfunc_controls"] -= 1
            write_jsonl(raw, records)

            with self.assertRaisesRegex(ValueError, "missing tfunc control"):
                report.load_complete_run(run_dir)

    def test_non_rust_workload_domain_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = make_run(directory)
            start_path = run_dir / "orchestrator.jsonl"
            start_records = [
                json.loads(line) for line in start_path.read_text().splitlines()
            ]
            start_records[0]["workloads"] = ["not-a-rust-workload"]
            write_jsonl(start_path, start_records)
            for policy in FEATURES:
                request_path = run_dir / "requests" / f"{policy}.json"
                request = json.loads(request_path.read_text())
                request["workloads"] = ["not-a-rust-workload"]
                write_json(request_path, request)
                raw_path = run_dir / "raw" / f"{policy}.jsonl"
                records = [
                    json.loads(line) for line in raw_path.read_text().splitlines()
                ]
                for record in records:
                    if "spec" in record:
                        record["spec"]["workload"] = "not-a-rust-workload"
                write_jsonl(raw_path, records)

            with self.assertRaisesRegex(ValueError, "Rust workload"):
                report.load_complete_run(run_dir)

    def test_rust_duration_and_integer_bounds_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = make_run(directory)
            request_path = run_dir / "requests" / "mvcc-optimistic.json"
            request = json.loads(request_path.read_text())
            request["measure_ms"] = 86_400_001
            write_json(request_path, request)
            with self.assertRaisesRegex(ValueError, "24 hours"):
                report.load_complete_run(run_dir)

        with tempfile.TemporaryDirectory() as directory:
            run_dir = make_run(directory)
            request_path = run_dir / "requests" / "optimistic.json"
            request = json.loads(request_path.read_text())
            request["seed"] = 1 << 64
            write_json(request_path, request)
            with self.assertRaisesRegex(ValueError, "Rust integer bound"):
                report.load_complete_run(run_dir)

    def test_non_rust_gc_mode_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = make_run(directory)
            raw_path = run_dir / "raw" / "mvcc-optimistic.jsonl"
            records = [json.loads(line) for line in raw_path.read_text().splitlines()]
            records[0]["gc_policy_mode"] = "version-aware"
            write_jsonl(raw_path, records)
            with self.assertRaisesRegex(ValueError, "GC policy mode"):
                report.load_complete_run(run_dir)

    def test_policy_requests_must_have_the_same_profile(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = make_run(directory)
            request_path = run_dir / "requests" / "mvcc-optimistic.json"
            request = json.loads(request_path.read_text())
            request["measure_ms"] = 999
            write_json(request_path, request)

            with self.assertRaisesRegex(ValueError, "benchmark profile"):
                report.load_complete_run(run_dir)

    def test_reports_have_stable_rows_sections_ratios_and_exact_totals(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = make_run(directory)
            data = report.generate_reports(run_dir)

            with (run_dir / "results.csv").open(newline="", encoding="utf-8") as stream:
                rows = list(csv.DictReader(stream))
            self.assertEqual(20, len(rows))
            self.assertEqual(
                ["optimistic", "optimistic", "optimistic"],
                [row["policy"] for row in rows[:3]],
            )
            self.assertEqual(
                8,
                sum(row["status"] == "completed" for row in rows),
            )
            self.assertEqual(4, sum(row["status"] == "skipped" for row in rows))
            self.assertEqual(8, sum(row["status"] == "control" for row in rows))

            expected = {"attempts": 3300, "committed": 3000, "aborts": 300, "retries": 300}
            self.assertEqual(expected, data.aggregate_counts)
            summary = (run_dir / "summary.md").read_text(encoding="utf-8")
            for text in (
                "optimistic",
                "mvcc-optimistic",
                "MVCC versus OCC",
                "Backend delta",
                "Scaling",
                "Abort and retry",
                "GC and versions",
                "tfunc controls",
                "Skipped cells",
                "Host and revision",
                "Dirty state",
                "Aggregate counts",
                "2.000x",
                "+100.0%",
                "12.500 seconds",
            ):
                self.assertIn(text, summary)
            self.assertIn("| attempts | 3300 |", summary)
            self.assertIn("| committed operations | 3000 |", summary)
            self.assertIn("| conflict aborts | 300 |", summary)
            self.assertIn("| retries | 300 |", summary)
            self.assertIn("| repetition | seed |", summary)

            merged = [
                json.loads(line)
                for line in (run_dir / "results.jsonl").read_text().splitlines()
            ]
            self.assertEqual("optimistic", merged[0]["compiled_policy"])
            self.assertEqual("mvcc-optimistic", merged[12]["compiled_policy"])

    def test_standalone_cli_rejects_external_output_but_api_allows_fixtures(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = make_run(Path(directory) / "fixture")
            report.generate_reports(run_dir)
            self.assertTrue((run_dir / "summary.md").is_file())

            repository_root = Path(directory) / "repository"
            repository_root.mkdir()
            with mock.patch.object(
                report, "REPOSITORY_ROOT", repository_root
            ), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(1, report.main([str(run_dir)]))


if __name__ == "__main__":
    unittest.main()
