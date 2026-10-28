#!/usr/bin/env python3
"""Measures how long each Cloudflare Worker takes to start, for CI.

Cloudflare rejects a deploy whose global scope takes more than one second to
run (the startup limit). Two figures are tracked:

- ``profile``: on every pull request, each Worker check bundles its Worker
  the way a deploy would and runs ``wrangler check startup`` on it several
  times. That profiles the startup phase locally in workerd, without a
  Cloudflare account. Single runs are noisy (the first one is a cold start),
  so the median active CPU time is reported and compared with the Worker's
  budget in ``scripts/worker_startup.toml``.
- ``deploy``: each deploy workflow on ``main`` passes wrangler's output here.
  ``wrangler deploy`` prints the startup time Cloudflare measured on its own
  servers, which is the figure the limit applies to.

Both write a Markdown table to the step summary and a ``::warning`` past a
budget. In ``report`` mode (the ``mode`` key) the script always exits 0; in
``enforce`` mode a Worker over budget fails the check. The parsers are sans-IO
so ``scripts/tests/test_worker_startup.py`` drives the same code.

Usage, from the repository root: ``worker_startup.py profile --worker site``
or ``worker_startup.py deploy --worker site`` with wrangler's output in
``$WRANGLER_OUTPUT``.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import re
import statistics
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

import tomllib

#: The budgets file, relative to the repository root.
CONFIG_FILE = "scripts/worker_startup.toml"
#: The line ``wrangler deploy`` prints with Cloudflare's startup figure.
DEPLOY_LINE_RE = re.compile(r"Worker Startup Time:\s*([0-9]{1,7}(?:\.[0-9]{1,3})?)\s*ms")
#: Cloudflare's startup limit, in milliseconds.
CLOUDFLARE_LIMIT_MS = 1000.0


@dataclass(frozen=True)
class Worker:
    """One Worker and its startup budget.

    Attributes:
        name: The Worker's short name (``site``, ``demo``, ``collector``).
        path: Its directory, relative to the repository root.
        limit_ms: The largest median local startup, in milliseconds, that
            passes; above it the check warns (or fails when enforcing).
        extra_args: Extra ``wrangler deploy --dry-run`` arguments that
            bundle it without a Cloudflare account.
    """

    name: str
    path: str
    limit_ms: float
    extra_args: tuple[str, ...] = ()


@dataclass(frozen=True)
class Config:
    """The budgets file.

    Attributes:
        enforce: Whether a Worker over budget fails the check.
        runs: How many local profiles each pull request run takes.
        deploy_warn_ms: The deploy-time startup, in milliseconds, past which
            a deploy warns; below Cloudflare's 1000 ms limit.
        workers: The Workers, by name.
    """

    enforce: bool
    runs: int
    deploy_warn_ms: float
    workers: dict[str, Worker]


class ConfigError(ValueError):
    """The budgets file is malformed."""


@dataclass(frozen=True)
class Startup:
    """One local startup profile, summarized the way ``wrangler check startup`` does.

    Attributes:
        active_ms: Sampled time not spent idle, in milliseconds, garbage
            collection included.
        gc_ms: Sampled time spent collecting garbage, in milliseconds.
        samples: How many samples the profile holds.
    """

    active_ms: float
    gc_ms: float
    samples: int


def parse_config(text: str) -> Config:
    """Read the budgets file.

    Args:
        text: The TOML text of ``scripts/worker_startup.toml``.

    Returns:
        The parsed budgets.

    Raises:
        ConfigError: A key is missing, has the wrong type, or is out of range.
    """
    try:
        data = tomllib.loads(text)
        mode = data["mode"]
        if mode not in ("report", "enforce"):
            raise ConfigError(f"mode must be report or enforce, not {mode!r}")
        runs = int(data["runs"])
        deploy_warn_ms = float(data["deploy_warn_ms"])
        workers = {}
        for name, entry in data["workers"].items():
            extra = entry.get("extra_args", [])
            if not all(isinstance(arg, str) for arg in extra):
                raise ConfigError(f"workers.{name}.extra_args must be strings")
            workers[name] = Worker(name, str(entry["path"]), float(entry["limit_ms"]), tuple(extra))
    except (tomllib.TOMLDecodeError, KeyError, TypeError, ValueError) as err:
        if isinstance(err, ConfigError):
            raise
        raise ConfigError(f"{CONFIG_FILE}: {err}") from err
    if not 1 <= runs <= 25 or not 0 < deploy_warn_ms <= CLOUDFLARE_LIMIT_MS:
        raise ConfigError("runs must be 1 to 25 and deploy_warn_ms 0 to 1000")
    if any(not 0 < worker.limit_ms <= CLOUDFLARE_LIMIT_MS for worker in workers.values()):
        raise ConfigError("every limit_ms must be above 0 and at most 1000")
    return Config(mode == "enforce", runs, deploy_warn_ms, workers)


def summarize_profile(text: str) -> Startup | None:
    """Summarize a ``.cpuprofile`` as wrangler's own startup summary does.

    The active time is the sum of the sample time deltas whose node is not
    ``(idle)``; ``(garbage collector)`` samples count as active and are also
    reported apart.

    Args:
        text: The JSON written by ``wrangler check startup --outfile``.

    Returns:
        The summary, or ``None`` when the text is not a usable profile.
    """
    try:
        profile = json.loads(text)
        names = {node["id"]: node["callFrame"]["functionName"] for node in profile["nodes"]}
        samples = profile.get("samples") or []
        deltas = profile.get("timeDeltas") or []
    except (ValueError, KeyError, TypeError):
        return None
    if not isinstance(samples, list) or not isinstance(deltas, list):
        return None
    sampled = idle = gc = 0.0
    for index, delta in enumerate(deltas):
        if isinstance(delta, bool) or not isinstance(delta, (int, float)) or not math.isfinite(delta) or delta < 0:
            return None
        name = names.get(samples[index]) if index < len(samples) else None
        sampled += delta
        if name == "(idle)":
            idle += delta
        elif name == "(garbage collector)":
            gc += delta
    return Startup(round((sampled - idle) / 1000, 1), round(gc / 1000, 1), len(samples))


def parse_deploy_output(text: str) -> float | None:
    """Find the startup time ``wrangler deploy`` reported, in milliseconds.

    Args:
        text: Everything wrangler printed during the deploy.

    Returns:
        The last reported figure, or ``None`` when wrangler printed none.
    """
    found = DEPLOY_LINE_RE.findall(text)
    return float(found[-1]) if found else None


def render_profiles(worker: Worker, runs: list[Startup]) -> tuple[str, float]:
    """Write one Worker's local profiles as a Markdown table.

    Args:
        worker: The Worker profiled.
        runs: Each run's summary, in order; at least one.

    Returns:
        The Markdown, and the median active time in milliseconds.
    """
    median = round(statistics.median(run.active_ms for run in runs), 1)
    verdict = "within budget" if median <= worker.limit_ms else "**over budget**"
    lines = [
        f"### Worker startup: {worker.name}",
        "",
        f"Median local startup **{median} ms** over {len(runs)} runs ({verdict}; budget {worker.limit_ms:g} ms,"
        f" Cloudflare's limit {CLOUDFLARE_LIMIT_MS:g} ms).",
        "",
        "| Run | Active (ms) | Garbage collection (ms) | Samples |",
        "| ---: | ---: | ---: | ---: |",
    ]
    lines += [f"| {n} | {run.active_ms} | {run.gc_ms} | {run.samples} |" for n, run in enumerate(runs, 1)]
    lines += [
        "",
        "Measured locally in workerd by `wrangler check startup`, so it is a trend, not Cloudflare's own figure.",
        "",
    ]
    return "\n".join(lines) + "\n", median


def _write_summary(markdown: str) -> None:
    """Append ``markdown`` to the step summary, or print it outside Actions."""
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as fh:
            fh.write(markdown)
    else:
        sys.stdout.write(markdown)


def _wrangler(args: list[str], cwd: Path) -> None:
    """Run the Worker's own wrangler, without telemetry.

    Raises:
        subprocess.CalledProcessError: wrangler failed.
    """
    env = {**os.environ, "WRANGLER_SEND_METRICS": "false"}
    subprocess.run(["npx", "--no-install", "wrangler", *args], cwd=cwd, env=env, check=True, stdout=subprocess.DEVNULL)


def profile(config: Config, worker: Worker) -> int:
    """Bundle ``worker``, profile its startup ``config.runs`` times and report.

    Returns:
        The exit status: 1 when enforcing and the median is over budget, or
        when wrangler produced no usable profile while enforcing.
    """
    level = "error" if config.enforce else "warning"
    cwd = Path(worker.path).resolve()
    with tempfile.TemporaryDirectory(prefix="worker-startup-") as tmp:
        bundle = Path(tmp) / "worker.bundle"
        runs = []
        try:
            _wrangler(["deploy", "--dry-run", *worker.extra_args, f"--outfile={bundle}"], cwd)
            for n in range(config.runs):
                out = Path(tmp) / f"run{n}.cpuprofile"
                _wrangler(["check", "startup", "--worker", str(bundle), "--outfile", str(out)], cwd)
                summary = summarize_profile(out.read_text(encoding="utf-8"))
                if summary is not None:
                    runs.append(summary)
        except (OSError, subprocess.CalledProcessError) as err:
            print(f"::{level} title=Worker startup::Could not profile {worker.name}: {err}")
            return 1 if config.enforce else 0
    if not runs:
        print(f"::{level} title=Worker startup::wrangler wrote no usable profile for {worker.name}.")
        return 1 if config.enforce else 0
    markdown, median = render_profiles(worker, runs)
    _write_summary(markdown)
    print(f"{worker.name}: median local startup {median} ms (budget {worker.limit_ms:g} ms)")
    if median > worker.limit_ms:
        print(
            f"::{level} title=Worker startup::{worker.name} takes {median} ms to start locally,"
            f" over its {worker.limit_ms:g} ms budget in {CONFIG_FILE}."
        )
        return 1 if config.enforce else 0
    return 0


def deploy(config: Config, worker: Worker, output: str) -> int:
    """Report the startup time Cloudflare measured for a deploy.

    A deploy has already happened, so this never fails the workflow: past
    ``deploy_warn_ms`` it warns.

    Returns:
        Always 0.
    """
    startup = parse_deploy_output(output)
    if startup is None:
        print(f"::notice title=Worker startup::wrangler printed no startup time for {worker.name}.")
        return 0
    _write_summary(
        f"### Worker startup: {worker.name}\n\nCloudflare measured **{startup:g} ms** for this deploy"
        f" (warning at {config.deploy_warn_ms:g} ms, limit {CLOUDFLARE_LIMIT_MS:g} ms).\n\n"
    )
    print(f"{worker.name}: Cloudflare startup {startup:g} ms")
    if startup > config.deploy_warn_ms:
        print(
            f"::warning title=Worker startup::{worker.name} took {startup:g} ms to start on Cloudflare,"
            f" past the {config.deploy_warn_ms:g} ms warning level; deploys fail at {CLOUDFLARE_LIMIT_MS:g} ms."
        )
    return 0


def main(argv: list[str] | None = None) -> int:
    """Run one subcommand.

    Args:
        argv: Command-line arguments; ``sys.argv[1:]`` when ``None``.

    Returns:
        The exit status.
    """
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("command", choices=("profile", "deploy"))
    parser.add_argument("--worker", required=True, help="the Worker's name in the budgets file")
    args = parser.parse_args(argv)
    config = parse_config(Path(CONFIG_FILE).read_text(encoding="utf-8"))
    worker = config.workers.get(args.worker)
    if worker is None:
        parser.error(f"unknown Worker {args.worker!r}; known: {', '.join(config.workers)}")
    if args.command == "profile":
        return profile(config, worker)
    return deploy(config, worker, os.environ.get("WRANGLER_OUTPUT", ""))


if __name__ == "__main__":
    sys.exit(main())
