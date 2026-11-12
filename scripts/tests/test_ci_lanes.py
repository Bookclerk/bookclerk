#!/usr/bin/env python3
"""Picking a lane for each heavy pull request CI job.

``scripts/ci_lanes.py`` sends each pull request job to the open lane with the
shortest line and lends extra lanes only while no merge group runs. These
tests drive its sans-IO readers with GitHub API bodies (the seed corpus is
trimmed from the runs waiting in the old per-job slots on 2026-11-11),
replay and mutate the corpus against independent oracles, check the lane
choice, and run its command line with a fake ``gh``.
"""

from __future__ import annotations

import copy
import importlib.util
import json
import os
import random
import subprocess
import sys
import tempfile
import unittest
import unittest.mock
from collections.abc import Sequence
from pathlib import Path
from types import ModuleType

ROOT = Path(__file__).resolve().parents[2]
CORPUS = Path(__file__).resolve().parent / "ci_lanes_corpus"
REPO = "Bookclerk/bookclerk"


def _load() -> ModuleType:
    spec = importlib.util.spec_from_file_location("ci_lanes", ROOT / "scripts" / "ci_lanes.py")
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["ci_lanes"] = module
    spec.loader.exec_module(module)
    return module


cl = _load()


def seed(name: str) -> bytes:
    return (CORPUS / name).read_bytes()


def edited_runs(name: str, index: int, **over: object) -> bytes:
    doc = json.loads(seed(name))
    doc["workflow_runs"][index].update(over)
    return json.dumps(doc).encode()


ACTIVE = {"queued", "in_progress", "pending", "waiting", "requested"}


def oracle_runs(raw: bytes) -> tuple[bool, list[int]] | None:
    """An independent walk of a run listing, with exact type checks."""
    try:
        doc = json.loads(raw)
    except Exception:  # noqa: BLE001 - any parse failure means "unreadable"
        return None
    if type(doc) is not dict or type(doc.get("workflow_runs")) is not list:
        return None
    queue, prs = False, []
    for run in doc["workflow_runs"]:
        if type(run) is not dict:
            return None
        if type(run.get("id")) is not int or type(run.get("event")) is not str or type(run.get("status")) is not str:
            return None
        repo = run.get("repository")
        if type(repo) is not dict or repo.get("full_name") != REPO:
            continue
        if run.get("path") != ".github/workflows/ci.yml" or run["status"] not in ACTIVE:
            continue
        if run["event"] == "merge_group":
            queue = True
        elif run["event"] == "pull_request" and run["id"] > 0:
            prs.append(run["id"])
    return queue, prs


def oracle_loads(raw: bytes) -> dict[int, int] | None:
    """An independent walk of a job listing: lane numbers parsed by hand."""
    try:
        doc = json.loads(raw)
    except Exception:  # noqa: BLE001
        return None
    if type(doc) is not dict or type(doc.get("jobs")) is not list:
        return None
    loads: dict[int, int] = {}
    for job in doc["jobs"]:
        if type(job) is not dict or type(job.get("name")) is not str or type(job.get("status")) is not str:
            return None
        if job["status"] not in ACTIVE:
            continue
        head, sep, tail = job["name"].rpartition(" · lane ")
        if not sep or not tail or len(tail) > 2 or not all(c in "0123456789" for c in tail):
            continue
        if len(tail) == 2 and tail[0] == "0":
            continue
        lane = int(tail)
        if lane < 17:
            loads[lane] = loads.get(lane, 0) + 1
    return loads


def check_against_oracles(case: unittest.TestCase, raw: bytes, label: str) -> None:
    with case.subTest(input=label):
        case.assertEqual(cl.active_runs(raw, REPO), oracle_runs(raw))
        case.assertEqual(cl.lane_loads(raw), oracle_loads(raw))


class ActiveRunsTests(unittest.TestCase):
    """Only unfinished CI runs of this repository count."""

    def test_reads_pull_request_runs(self) -> None:
        queue, prs = cl.active_runs(seed("runs__pending_prs.json"), REPO)
        self.assertFalse(queue)
        self.assertEqual(len(prs), 4)
        self.assertEqual(cl.active_runs(seed("runs__empty.json"), REPO), (False, []))

    def test_an_active_merge_group_marks_the_queue_busy(self) -> None:
        self.assertEqual(cl.active_runs(seed("runs__merge_group_active.json"), REPO), (True, []))
        done = edited_runs("runs__merge_group_active.json", 0, status="completed", conclusion="success")
        self.assertEqual(cl.active_runs(done, REPO), (False, []))

    def test_skips_other_runs(self) -> None:
        cases = {
            "other workflow": edited_runs("runs__merge_group_active.json", 0, path=".github/workflows/codeql.yml"),
            "other repository": edited_runs("runs__merge_group_active.json", 0, repository={"full_name": "x/y"}),
            "push": edited_runs("runs__merge_group_active.json", 0, event="push"),
        }
        for name, raw in cases.items():
            with self.subTest(case=name):
                self.assertEqual(cl.active_runs(raw, REPO), (False, []))
        self.assertEqual(cl.active_runs(seed("runs__pending_prs.json"), "other/repo"), (False, []))

    def test_malformed_listings_are_unreadable(self) -> None:
        bad = {
            "not json": b"{",
            "list": b"[]",
            "no runs": b"{}",
            "id not int": edited_runs("runs__pending_prs.json", 0, id="1"),
            "id bool": edited_runs("runs__pending_prs.json", 0, id=True),
            "status missing": edited_runs("runs__pending_prs.json", 0, status=None),
            "entry not object": json.dumps({"workflow_runs": [1]}).encode(),
        }
        for name, raw in bad.items():
            with self.subTest(case=name):
                self.assertIsNone(cl.active_runs(raw, REPO))


class LaneLoadsTests(unittest.TestCase):
    """Active jobs count in the lane their name ends in."""

    def test_counts_active_laned_jobs(self) -> None:
        # Two pending jobs in lane 3, one running in lane 12; the completed
        # job in lane 5 and the unlabelled jobs don't count.
        self.assertEqual(cl.lane_loads(seed("jobs__laned.json")), {3: 2, 12: 1})
        self.assertEqual(cl.lane_loads(seed("jobs__unlabelled.json")), {})

    def test_label_must_end_the_name_and_name_a_lane(self) -> None:
        for name in ("test · lane 17", "test · lane 03", "test · lane 3 ", "test · lane 3x", "test lane 3", "test · lane -1"):
            raw = json.dumps({"jobs": [{"name": name, "status": "pending"}]}).encode()
            with self.subTest(name=name):
                self.assertEqual(cl.lane_loads(raw), {})
        raw = json.dumps({"jobs": [{"name": "a · lane 1 · lane 16", "status": "queued"}]}).encode()
        self.assertEqual(cl.lane_loads(raw), {16: 1})

    def test_malformed_listings_are_unreadable(self) -> None:
        for raw in (b"", b"[]", b'{"jobs": {}}', b'{"jobs": [{"name": 1, "status": "queued"}]}', b'{"jobs": [null]}'):
            with self.subTest(raw=raw):
                self.assertIsNone(cl.lane_loads(raw))


class PlannedLegsTests(unittest.TestCase):
    """One key per leg of each planned lane job."""

    def test_keys_follow_the_plan(self) -> None:
        outputs = {
            "job_test": "true",
            "job_check": "true",
            "job_postgres": "true",
            "job_native_gateway": "true",
            "job_site": "false",
            "job_windows_cluster": "true",
            "os_matrix": '["ubuntu-latest","windows-latest"]',
        }
        self.assertEqual(
            cl.planned_legs(outputs),
            ["test-0", "check-0", "postgres-0", "postgres-1", "postgres-2", "native-gateway-0", "native-gateway-1"],
        )

    def test_a_bad_os_matrix_means_one_leg(self) -> None:
        for matrix in (None, "", "[]", "{", "{}", 3):
            with self.subTest(matrix=matrix):
                self.assertEqual(cl.planned_legs({"job_tray": "true", "os_matrix": matrix}), ["tray-0"])

    def test_draft_plan(self) -> None:
        self.assertEqual(cl.planned_legs({"job_check": "true", "job_test": "true"}), ["test-0", "check-0"])
        self.assertEqual(cl.planned_legs({}), [])


class AssignTests(unittest.TestCase):
    """The shortest open line wins; lanes are lent only to an idle queue."""

    KEYS = [f"job-{i}" for i in range(12)]

    def test_spreads_over_base_lanes_before_lending(self) -> None:
        picked = cl.assign(self.KEYS[:9], {}, False, 7)
        self.assertEqual(sorted(picked.values()), list(range(cl.BASE_LANES)))

    def test_lends_empty_lanes_when_the_queue_is_idle(self) -> None:
        picked = cl.assign(self.KEYS, {}, False, 7)
        lent = [lane for lane in picked.values() if lane >= cl.BASE_LANES]
        self.assertEqual(len(lent), 3)
        self.assertEqual(len(set(lent)), 3, "a lent lane holds one job")

    def test_never_lends_while_the_queue_is_busy(self) -> None:
        picked = cl.assign(self.KEYS, {}, True, 7)
        self.assertTrue(all(lane < cl.BASE_LANES for lane in picked.values()))

    def test_never_lends_without_loads(self) -> None:
        picked = cl.assign(self.KEYS, None, False, 7)
        self.assertTrue(all(lane < cl.BASE_LANES for lane in picked.values()))

    def test_never_lends_a_busy_lane(self) -> None:
        loads = {lane: 3 for lane in range(cl.BASE_LANES)} | {lane: 1 for lane in range(cl.BASE_LANES, cl.ALL_LANES)}
        picked = cl.assign(self.KEYS[:2], loads, False, 7)
        self.assertTrue(all(lane < cl.BASE_LANES for lane in picked.values()))

    def test_picks_the_shortest_line(self) -> None:
        loads = {lane: 2 for lane in range(cl.BASE_LANES)}
        loads[4] = 0
        self.assertEqual(cl.assign(["test-0"], loads, True, 1), {"test-0": 4})

    def test_ties_break_by_run(self) -> None:
        firsts = {cl.assign(["test-0"], {}, True, run)["test-0"] for run in range(1, 200)}
        self.assertGreater(len(firsts), 5)

    def test_load_invariant(self) -> None:
        """Each pick is a minimum over the lanes open at that moment."""
        rng = random.Random(11)
        for _ in range(500):
            loads = {lane: rng.randrange(4) for lane in range(cl.ALL_LANES) if rng.random() < 0.7}
            queue = rng.random() < 0.5
            keys = [f"k-{i}" for i in range(rng.randrange(1, 15))]
            picked = cl.assign(keys, loads, queue, rng.randrange(1, 10**9))
            count = {lane: loads.get(lane, 0) for lane in range(cl.ALL_LANES)}
            for key in keys:
                lane = picked[key]
                open_lanes = list(range(cl.BASE_LANES))
                if not queue:
                    open_lanes += [n for n in range(cl.BASE_LANES, cl.ALL_LANES) if count[n] == 0]
                self.assertIn(lane, open_lanes)
                self.assertEqual(count[lane], min(count[n] for n in open_lanes))
                count[lane] += 1
            for lane in range(cl.BASE_LANES, cl.ALL_LANES):
                self.assertLessEqual(count[lane] - loads.get(lane, 0), 1 if loads.get(lane, 0) == 0 else 0)

    def test_output_shape(self) -> None:
        out = json.loads(cl.lanes_output({"test-0": 3, "postgres-2": 12}))
        self.assertEqual(
            out,
            {
                "postgres-2": {"group": "ci-pr-lane-12", "label": " · lane 12"},
                "test-0": {"group": "ci-pr-lane-3", "label": " · lane 3"},
            },
        )
        self.assertTrue(cl.lanes_output({"test-0": 3}).isascii())


class FakeGh:
    """Answers `gh api` calls from a table; records them."""

    def __init__(self, table: dict[str, bytes], fail: str | None = None) -> None:
        self.table, self.fail, self.calls = table, fail, []

    def __call__(self, args: Sequence[str]) -> bytes:
        self.calls.append(args[1])
        if self.fail and self.fail in args[1]:
            raise subprocess.CalledProcessError(1, ["gh", *args])
        for prefix, body in self.table.items():
            if args[1].startswith(prefix):
                return body
        return seed("runs__empty.json")


class MainTests(unittest.TestCase):
    """The command line writes the lanes and falls back on API trouble."""

    OUTPUTS = json.dumps({"job_test": "true", "job_check": "true", "os_matrix": '["ubuntu-latest"]'})

    def run_main(self, gh: FakeGh, outputs: str | None = None) -> dict:
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "out"
            summary = Path(tmp) / "summary"
            env = {"GITHUB_OUTPUT": str(out), "GITHUB_STEP_SUMMARY": str(summary)}
            with unittest.mock.patch.dict(os.environ, env):
                code = cl.main(
                    ["--repo", REPO, "--run-id", "99", "--outputs", outputs or self.OUTPUTS], gh=gh
                )
            self.assertEqual(code, 0)
            line = out.read_text(encoding="utf-8")
            self.assertTrue(line.startswith("lanes="))
            self.assertIn("Pull request lanes:", summary.read_text(encoding="utf-8"))
            return json.loads(line.removeprefix("lanes="))

    def test_counts_other_runs_lanes(self) -> None:
        pending = json.loads(seed("runs__pending_prs.json"))
        pending["workflow_runs"] = pending["workflow_runs"][:1]
        run_id = pending["workflow_runs"][0]["id"]
        busy = {"jobs": [{"name": f"x · lane {n}", "status": "pending"} for n in range(cl.BASE_LANES) if n != 6]}
        gh = FakeGh(
            {
                f"repos/{REPO}/actions/workflows/ci.yml/runs?status=pending": json.dumps(pending).encode(),
                f"repos/{REPO}/actions/runs/{run_id}/jobs": json.dumps(busy).encode(),
            }
        )
        lanes = self.run_main(gh)
        # The empty base lane first, then a lent lane (the queue is idle).
        self.assertEqual(lanes["test-0"]["group"], "ci-pr-lane-6")
        self.assertGreaterEqual(int(lanes["check-0"]["group"].rsplit("-", 1)[1]), cl.BASE_LANES)
        self.assertEqual(len([c for c in gh.calls if "/jobs" in c]), 1)

    def test_busy_queue_lends_nothing(self) -> None:
        gh = FakeGh(
            {f"repos/{REPO}/actions/workflows/ci.yml/runs?status=in_progress": seed("runs__merge_group_active.json")}
        )
        many = json.dumps({"job_test": "true", "job_check": "true", "job_postgres": "true", "job_e2e": "true",
                           "job_release": "true", "job_site": "true", "job_tray": "true", "job_confinement": "true",
                           "job_native_gateway": "true", "os_matrix": '["ubuntu-latest"]'})  # fmt: skip
        lanes = self.run_main(gh, many)
        self.assertEqual(len(lanes), 11)
        self.assertTrue(all(int(v["group"].rsplit("-", 1)[1]) < cl.BASE_LANES for v in lanes.values()))

    def test_api_failure_falls_back_to_base_lanes(self) -> None:
        for fail in ("status=pending", "/jobs"):
            pending = seed("runs__pending_prs.json")
            gh = FakeGh({f"repos/{REPO}/actions/workflows/ci.yml/runs?status=pending": pending}, fail=fail)
            with self.subTest(fail=fail):
                lanes = self.run_main(gh, json.dumps({k: "true" for k in ("job_test", "job_check", "job_e2e")}
                                                     | {"job_postgres": "true"}))  # fmt: skip
                self.assertEqual(len(lanes), 6)
                self.assertTrue(all(int(v["group"].rsplit("-", 1)[1]) < cl.BASE_LANES for v in lanes.values()))

    def test_nothing_planned_reads_nothing(self) -> None:
        gh = FakeGh({})
        self.assertEqual(self.run_main(gh, "{}"), {})
        self.assertEqual(gh.calls, [])

    def test_bad_arguments(self) -> None:
        for argv in (
            ["--repo", "bad repo", "--run-id", "1", "--outputs", "{}"],
            ["--repo", REPO, "--run-id", "0", "--outputs", "{}"],
            ["--repo", REPO, "--run-id", "1", "--outputs", "[]"],
        ):
            with self.subTest(argv=argv):
                self.assertEqual(cl.main(argv, gh=FakeGh({})), 2)


class CorpusTests(unittest.TestCase):
    """Every seed replays against the oracles."""

    def test_replay(self) -> None:
        for path in sorted(CORPUS.iterdir()):
            with self.subTest(seed=path.name):
                self.assertIn(path.name.split("__")[0], ("runs", "jobs"))
                check_against_oracles(self, path.read_bytes(), path.name)


class FuzzTests(unittest.TestCase):
    """Seeded mutation fuzzing: never raises, always agrees with the oracles."""

    VALUES: list[object] = [
        None, True, False, 0, 1, -1, 1.5, "", "x", "pending", "queued", "in_progress", "completed",
        "merge_group", "pull_request", "push", ".github/workflows/ci.yml", "t · lane 3", "t · lane 16",
        "t · lane 17", "t · lane 07", {"full_name": REPO}, REPO, [], {}, [1], {"a": 1},
    ]  # fmt: skip

    def mutate_json(self, rng: random.Random, node: object) -> object:
        if isinstance(node, dict) and node and rng.random() < 0.75:
            key = rng.choice(list(node))
            out = dict(node)
            if rng.random() < 0.2:
                del out[key]
            else:
                out[key] = self.mutate_json(rng, node[key])
            return out
        if isinstance(node, list) and node and rng.random() < 0.75:
            out = list(node)
            i = rng.randrange(len(out))
            out[i] = self.mutate_json(rng, out[i])
            return out
        if isinstance(node, str) and node and rng.random() < 0.5:
            i = rng.randrange(len(node))
            return node[:i] + rng.choice("0159aZ- ·é") + node[i + 1 :]
        return copy.deepcopy(rng.choice(self.VALUES))

    def mutate_bytes(self, rng: random.Random, raw: bytes) -> bytes:
        data = bytearray(raw)
        for _ in range(rng.randint(1, 4)):
            pos = rng.randint(0, len(data))
            op = rng.random()
            if op < 0.4:
                data[pos:pos] = bytes([rng.choice(b'{}[]",:0aA \\\xff')])
            elif op < 0.7 and data:
                del data[min(pos, len(data) - 1)]
            elif data:
                data[min(pos, len(data) - 1)] = rng.randrange(256)
        return bytes(data)

    def test_structural_mutations(self) -> None:
        rng = random.Random(20261111)
        seeds = [json.loads(p.read_bytes()) for p in sorted(CORPUS.iterdir())]
        for i in range(3000):
            node = rng.choice(seeds)
            for _ in range(rng.randint(1, 3)):
                node = self.mutate_json(rng, node)
            raw = json.dumps(node).encode()
            check_against_oracles(self, raw, f"iteration {i}: {raw[:300]!r}")

    def test_byte_mutations(self) -> None:
        rng = random.Random(864)
        seeds = [p.read_bytes() for p in sorted(CORPUS.iterdir())]
        for i in range(3000):
            raw = self.mutate_bytes(rng, rng.choice(seeds))
            check_against_oracles(self, raw, f"iteration {i}: {raw[:300]!r}")


if __name__ == "__main__":
    unittest.main()
