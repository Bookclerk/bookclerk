#!/usr/bin/env python3
"""Envelope cgroup classification and CI assertions."""

from __future__ import annotations

import importlib.util
import io
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BENCH = ROOT / "scripts" / "bench"


def _load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


limits = _load("envelope_limits", BENCH / "envelope_limits.py")
assertions = _load("assert_envelope", BENCH / "assert_envelope.py")


def _write(directory: Path, memory: str | None, cpu: str | None) -> tuple[str, str]:
    mem = directory / "memory.max"
    cpu_path = directory / "cpu.max"
    if memory is not None:
        mem.write_text(memory + "\n", encoding="utf-8")
    if cpu is not None:
        cpu_path.write_text(cpu + "\n", encoding="utf-8")
    return str(mem), str(cpu_path)


class LimitTests(unittest.TestCase):
    def test_one_gib_one_core_is_the_envelope(self) -> None:
        self.assertTrue(limits.within_envelope("1073741824", "100000 100000"))
        self.assertTrue(limits.within_envelope("536870912", "50000 100000"))

    def test_max_or_looser_is_not(self) -> None:
        self.assertFalse(limits.within_envelope("max", "100000 100000"))
        self.assertFalse(limits.within_envelope("1073741824", "max"))
        self.assertFalse(limits.within_envelope("2147483648", "100000 100000"))
        self.assertFalse(limits.within_envelope("1073741824", "200000 100000"))
        self.assertFalse(limits.within_envelope("", ""))

    def test_classify_reads_controller_files(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            mem, cpu = _write(directory, "1073741824", "100000 100000")
            out = io.StringIO()
            with redirect_stdout(out):
                self.assertEqual(limits.main(["envelope_limits.py", "classify", mem, cpu]), 0)
            self.assertEqual(out.getvalue().strip(), "accept")
            self.assertEqual(limits.main(["envelope_limits.py", "check", mem, cpu]), 0)
            loose, cpu = _write(directory, "max", "100000 100000")
            self.assertEqual(limits.main(["envelope_limits.py", "check", loose, cpu]), 1)
            missing = str(directory / "absent.max")
            out = io.StringIO()
            with redirect_stdout(out):
                self.assertEqual(limits.main(["envelope_limits.py", "classify", missing, missing]), 0)
            self.assertEqual(out.getvalue().strip(), "delegate")


class AssertionTests(unittest.TestCase):
    def _doc(self, **overrides):
        doc = {
            "available_parallelism": 1,
            "affinity_cpus": 4,
            "os_cpu_count": 4,
            "cpu_max": "100000 100000",
            "memory_max": "1073741824",
            "media_pool_line": "media pool: 1 workers, confinement=required",
            "windows": {
                "idle": {
                    "memory_peak": 736_337_920,
                    "anon": 30_994_432,
                    "memory_current": 33_607_680,
                    "startup_ms": 517,
                },
                "api": {
                    "per_route": {
                        "GET /health": {"errors": 0, "p50_ms": 50_000, "min_total": None},
                        "GET /api/library/books?limit=40&offset=0": {
                            "errors": 0,
                            "p50_ms": 80_000,
                            "min_total": 10_000,
                        },
                        "GET /api/library/books?q=Title&limit=40": {
                            "errors": 0,
                            "p50_ms": 90_000,
                            "min_total": 500,
                        },
                        "GET /api/library/books?q=Title&limit=8": {
                            "errors": 0,
                            "p50_ms": 90_000,
                            "min_total": 8,
                        },
                    },
                    "mix_60s": {"requests": 100, "errors": 0},
                    "sample": {"memory_peak": 736_337_920, "anon": 57_602_048},
                },
                "api_with_rebuild": {
                    "indexed": 10_000,
                    "mix_60s": {"requests": 80, "errors": 0},
                    "sample": {"memory_peak": 736_337_920, "anon": 65_904_640},
                },
            },
        }
        doc.update(overrides)
        return doc

    def test_measured_shape_passes_without_a_latency_gate(self) -> None:
        self.assertEqual(assertions.assert_envelope(self._doc()), [])

    def test_books_errors_peak_pool_and_empty_index_fail(self) -> None:
        books = self._doc()
        books["windows"]["api"]["per_route"]["GET /api/library/books?limit=40&offset=0"]["errors"] = 2
        self.assertTrue(any("non-200" in item for item in assertions.assert_envelope(books)))

        empty = self._doc()
        empty["windows"]["api"]["per_route"]["GET /api/library/books?q=Title&limit=40"]["min_total"] = 0
        self.assertTrue(any("populated index" in item for item in assertions.assert_envelope(empty)))

        peak = self._doc()
        peak["windows"]["api"]["sample"]["anon"] = 256 * 1024 * 1024
        self.assertTrue(any("256 MiB" in item for item in assertions.assert_envelope(peak)))

        current = self._doc()
        del current["windows"]["idle"]["anon"]
        current["windows"]["idle"]["memory_current"] = 33_607_680
        self.assertEqual(assertions.assert_envelope(current), [])

        missing = self._doc()
        del missing["windows"]["idle"]["anon"]
        del missing["windows"]["idle"]["memory_current"]
        self.assertTrue(any("anon" in item for item in assertions.assert_envelope(missing)))

        pool = self._doc()
        pool["media_pool_line"] = "media pool: 4 workers, confinement=required"
        self.assertTrue(any("media pool" in item for item in assertions.assert_envelope(pool)))

        missing = self._doc()
        del missing["affinity_cpus"]
        self.assertTrue(any("affinity" in item for item in assertions.assert_envelope(missing)))


class ContainerScriptTests(unittest.TestCase):
    def test_docker_run_is_the_envelope_and_does_not_compile(self) -> None:
        host = (BENCH / "envelope-container.sh").read_text(encoding="utf-8")
        inside = (BENCH / "envelope-inside.sh").read_text(encoding="utf-8")
        self.assertIn("--memory=1g", host)
        self.assertIn("--memory-swap=1g", host)
        self.assertIn("--cpus=1", host)
        self.assertIn("ubuntu:24.04", host)
        self.assertNotIn("slim", host)
        self.assertIn("bash ", host)
        self.assertIn("envelope-inside.sh", host)
        self.assertTrue(inside.startswith("#!/usr/bin/env bash\n"))
        self.assertIn("set -euo pipefail", inside)
        self.assertNotIn("cargo", inside)
        self.assertNotIn("rustc", inside)
        self.assertIn("small-vps.sh", inside)


if __name__ == "__main__":
    unittest.main()
