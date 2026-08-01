import contextlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).parents[1] / "transaction-cc-bench.py"
SPEC = importlib.util.spec_from_file_location("transaction_cc_bench", SCRIPT)
bench = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = bench
SPEC.loader.exec_module(bench)


class TransactionCcBenchTests(unittest.TestCase):
    def test_mvcc_command_has_exactly_optimistic_and_mvcc(self):
        cmd = bench.build_command(
            "mvcc-optimistic", Path("request.json"), Path("out.jsonl")
        )
        features = cmd[cmd.index("--features") + 1].split(",")

        self.assertIn("transaction-mvcc", features)
        self.assertIn("transaction-cc-optimistic-validation", features)
        self.assertEqual(
            1, len([f for f in features if f.startswith("transaction-cc-")])
        )

    def test_single_version_commands_have_one_cc_and_no_mvcc(self):
        for policy in bench.POLICIES:
            if policy == "mvcc-optimistic":
                continue
            with self.subTest(policy=policy):
                features = bench.features_for(policy)
                self.assertNotIn("transaction-mvcc", features)
                self.assertEqual(
                    1,
                    len([f for f in features if f.startswith("transaction-cc-")]),
                )

    def test_policy_mapping_and_complete_feature_closure_are_exact(self):
        self.assertEqual(
            {
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
            },
            bench.POLICIES,
        )
        self.assertEqual(
            (
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
            ),
            bench.BASE_FEATURES,
        )

    def test_build_command_is_the_release_ignored_driver(self):
        command = bench.build_command(
            "lockbased", Path("request.json"), Path("out.jsonl")
        )

        self.assertEqual(
            [
                "cargo",
                "test",
                "-q",
                "-p",
                "wasmtime",
                "--release",
                "--no-default-features",
                "--features",
                ",".join((*bench.BASE_FEATURES, "transaction-cc-lockbased")),
                "transaction_cc_benchmark_driver",
                "--lib",
                "--",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ],
            command,
        )

    def test_request_serialization_is_stable_and_uses_cli_filters(self):
        args = bench.parse_args(
            [
                "--policies",
                "lockbased",
                "--backends",
                "file-backed,vmemory",
                "--workloads",
                "hot-key,read-only",
                "--workers",
                "2,1",
                "--warmup-ms",
                "7",
                "--measure-ms",
                "11",
                "--repetitions",
                "3",
                "--seed",
                "0x2a",
                "--gc-commit-interval",
                "17",
                "--skip-tfunc-control",
            ]
        )
        request = bench.request_for("lockbased", args)

        self.assertEqual(
            {
                "schema_version": 1,
                "expected_policy": "lockbased",
                "warmup_ms": 7,
                "measure_ms": 11,
                "repetitions": 3,
                "backends": ["file-backed", "vmemory"],
                "workloads": ["hot-key", "read-only"],
                "workers": [2, 1],
                "seed": 42,
                "gc_commit_interval": 17,
                "include_tfunc_control": False,
            },
            request,
        )
        with tempfile.TemporaryDirectory() as directory:
            first = Path(directory) / "first.json"
            second = Path(directory) / "second.json"
            bench.write_request(first, request)
            bench.write_request(second, request)
            self.assertEqual(first.read_bytes(), second.read_bytes())
            self.assertTrue(first.read_bytes().endswith(b"\n"))

    def test_invalid_filters_are_rejected_before_cargo(self):
        invalid_argvs = [
            ["--policies", "unknown"],
            ["--backends", "dax"],
            ["--workloads", "unknown"],
            ["--workers", "0"],
            ["--warmup-ms", "0"],
            ["--gc-commit-interval", "0"],
        ]
        for argv in invalid_argvs:
            with self.subTest(argv=argv), contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit):
                    bench.parse_args(argv)

    def test_third_policy_failure_preserves_raw_data_and_failure_metadata(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = Path(directory) / "run"
            args = bench.parse_args(
                [
                    "--policies",
                    "lockbased,no-wait,optimistic,strict-2pl",
                    "--backends",
                    "vmemory",
                    "--workloads",
                    "read-only",
                    "--workers",
                    "1",
                    "--warmup-ms",
                    "1",
                    "--measure-ms",
                    "1",
                    "--output-dir",
                    str(run_dir),
                    "--skip-tfunc-control",
                ]
            )
            cargo_calls = []

            def fake_run(command, **kwargs):
                if command[:3] == ["git", "rev-parse", "HEAD"]:
                    return subprocess.CompletedProcess(
                        command, 0, "0123456789abcdef0123456789abcdef01234567\n", ""
                    )
                if command[:2] == ["git", "status"]:
                    return subprocess.CompletedProcess(
                        command, 0, " M tracked-file\n?? untracked-file\n", ""
                    )
                if command[:2] == ["rustc", "-Vv"]:
                    return subprocess.CompletedProcess(command, 0, "rustc test\n", "")
                if command[0] == "uname":
                    return subprocess.CompletedProcess(command, 0, "test-processor\n", "")
                self.assertEqual("cargo", command[0])
                cargo_calls.append(command)
                raw_path = Path(kwargs["env"]["WASMTIME_TRANSACTION_BENCH_OUTPUT"])
                line = json.dumps(
                    {
                        "record_type": "complete" if len(cargo_calls) < 3 else "failure",
                        "policy_call": len(cargo_calls),
                    }
                )
                raw_path.write_text(line + "\n", encoding="utf-8")
                return subprocess.CompletedProcess(
                    command, 23 if len(cargo_calls) == 3 else 0
                )

            with mock.patch.object(bench.subprocess, "run", side_effect=fake_run):
                status = bench.run(args, argv=["transaction-cc-bench.py", "--test"])

            self.assertEqual(1, status)
            self.assertEqual(3, len(cargo_calls))
            raw_dir = run_dir / "raw"
            for index, policy in enumerate(
                ("lockbased", "no-wait", "optimistic"), start=1
            ):
                raw = raw_dir / f"{policy}.jsonl"
                self.assertTrue(raw.is_file(), raw)
                self.assertEqual(index, json.loads(raw.read_text())["policy_call"])
            self.assertFalse((raw_dir / "strict-2pl.jsonl").exists())

            records = [
                json.loads(line)
                for line in (run_dir / "orchestrator.jsonl").read_text().splitlines()
            ]
            self.assertEqual("policy_failure", records[-1]["record_type"])
            self.assertEqual("optimistic", records[-1]["policy"])
            self.assertEqual(23, records[-1]["returncode"])
            self.assertNotIn("complete", [record["record_type"] for record in records])

            invocation = json.loads((run_dir / "invocation.json").read_text())
            self.assertEqual("failed", invocation["status"])
            self.assertEqual("optimistic", invocation["failed_policy"])
            self.assertEqual(
                ["tracked-file", "untracked-file"], invocation["changed_paths"]
            )
            self.assertTrue(invocation["dirty"])


if __name__ == "__main__":
    unittest.main()
