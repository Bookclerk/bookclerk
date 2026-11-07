#!/usr/bin/env python3
"""Which commits change what goes into a published image or the demo.

``images-publish.yml`` runs after every push to main and builds an image
only when something it is made of changed since the last commit that has a
published image; ``demo-deploy.yml`` follows it and deploys the newest
published demo image built from the same inputs. Both ask this script,
so they agree on what an image is made of.

Each kind of input is a set of Git pathspecs:

- ``app``: the production image ``bookclerk`` and the demo image
  ``bookclerk-demo``. ``packaging/docker/Dockerfile`` copies the whole
  repository into the builder, so this is everything except paths no build
  reads (docs, CI configuration, the Workers, root Markdown). Listing too
  much only costs a build; listing too little would publish a stale image,
  so ``scripts/tests/test_image_inputs.py`` checks that no crate embeds an
  excluded file.
- ``dev``: the Dev Container image ``bookclerk-dev``.
- ``demo``: what a demo deploy ships, the ``app`` inputs plus the demo
  Worker and its deploy workflow.

Usage:

- ``image_inputs.py candidates KIND SHA`` prints, newest first, ``SHA`` and
  the commits before it back to the last one that changed ``KIND``'s
  inputs: an image built from any of them is ``SHA``'s image. At most
  ``MAX_CANDIDATES``.
- ``image_inputs.py unchanged KIND FROM TO`` exits 0 when ``FROM`` is
  ``TO`` or an ancestor of it and no commit between them changed ``KIND``'s
  inputs, and 1 otherwise (including when ``FROM`` is unknown).
"""

from __future__ import annotations

import re
import subprocess
import sys

#: A full Git commit id, as GitHub reports one.
COMMIT = re.compile(r"[0-9a-f]{40}", re.ASCII)

#: The most commits ``candidates`` lists. Past it the caller builds the
#: image again rather than look further back.
MAX_CANDIDATES = 30

#: Paths no image build reads.
APP_EXCLUDES = (
    "docs",
    ".github",
    "tools/demo",
    "tools/diagnostics-collector",
    "tools/site",
    "tools/lint",
    "tools/codecov",
    "scripts/tests",
)

#: Pathspec groups for each kind of input: a commit changes the inputs when
#: it touches what any one group matches. Groups, because in one pathspec an
#: exclusion wins over every inclusion.
SPECS: dict[str, tuple[tuple[str, ...], ...]] = {
    "app": (
        (
            ".",
            *(f":(exclude){path}" for path in APP_EXCLUDES),
            # Root Markdown only (README.md, AGENTS.md, ...); a crate's own
            # README stays an input.
            ":(exclude,glob)*.md",
        ),
    ),
    "dev": (
        (
            ".devcontainer",
            "scripts/dev-env",
            "scripts/tests/requirements.txt",
            ".tool-versions",
        ),
    ),
}
SPECS["demo"] = (*SPECS["app"], ("tools/demo", ".github/workflows/demo-deploy.yml"))


class InputsError(ValueError):
    """An argument is not a known kind or a full commit id."""


def spec(kind: str) -> tuple[tuple[str, ...], ...]:
    """Returns the pathspec groups of one kind of input.

    # Arguments
    * ``kind``: ``app``, ``dev`` or ``demo``.

    # Errors
    ``InputsError`` for any other kind.
    """
    try:
        return SPECS[kind]
    except KeyError:
        raise InputsError(f"unknown kind {kind!r}; expected one of {', '.join(SPECS)}") from None


def _commit(text: str) -> str:
    """Returns ``text`` when it is a full lowercase commit id.

    # Errors
    ``InputsError`` otherwise, so nothing else reaches ``git``'s argv.
    """
    if COMMIT.fullmatch(text) is None:
        raise InputsError(f"not a full commit id: {text!r}")
    return text


def _git(*args: str) -> subprocess.CompletedProcess[str]:
    """Runs ``git`` in the current repository and captures its output."""
    return subprocess.run(["git", *args], capture_output=True, text=True, check=False)


def candidates(kind: str, sha: str) -> list[str]:
    """Lists the commits whose image of ``kind`` equals ``sha``'s.

    # Arguments
    * ``kind``: the kind of input.
    * ``sha``: the commit an image is wanted for.

    # Returns
    ``sha``, then its first-parent ancestors with the same inputs, newest
    first, at most ``MAX_CANDIDATES``. The last one listed is the commit
    that last changed the inputs when it is within reach.

    # Errors
    ``InputsError`` for a bad argument; ``RuntimeError`` when ``git`` fails.
    """
    spec(kind)
    sha = _commit(sha)
    walk = _git("rev-list", "--first-parent", f"--max-count={MAX_CANDIDATES}", sha)
    if walk.returncode != 0:
        raise RuntimeError(walk.stderr.strip() or "git rev-list failed")
    found = [sha]
    for commit in walk.stdout.split()[1:]:
        # Stops at the first commit whose inputs differ, so a change merged
        # through a second parent is never skipped over.
        if not unchanged(kind, commit, sha):
            break
        found.append(commit)
    return found


def unchanged(kind: str, base: str, head: str) -> bool:
    """Whether ``head`` has the same ``kind`` inputs as ``base``.

    # Arguments
    * ``kind``: the kind of input.
    * ``base``: the earlier commit, for example the one the live demo runs.
    * ``head``: the later commit.

    # Returns
    True when ``base`` is ``head`` or an ancestor of it and no commit
    between them changed ``kind``'s inputs; False otherwise, including when
    ``base`` is not in the repository.

    # Errors
    ``InputsError`` for a bad argument.
    """
    groups = spec(kind)
    base, head = _commit(base), _commit(head)
    if _git("merge-base", "--is-ancestor", base, head).returncode != 0:
        return False
    for pathspecs in groups:
        log = _git("log", "-1", "--format=%H", f"{base}..{head}", "--", *pathspecs)
        if log.returncode != 0 or log.stdout.strip():
            return False
    return True


def main(argv: list[str]) -> int:
    """Runs one command and prints its result.

    # Arguments
    * ``argv``: the arguments after the program name.

    # Returns
    0 on success (for ``unchanged``: unchanged), 1 for ``unchanged`` when
    something changed or on a ``git`` failure, 2 for usage.
    """
    try:
        if len(argv) == 3 and argv[0] == "candidates":
            print("\n".join(candidates(argv[1], argv[2])))
            return 0
        if len(argv) == 4 and argv[0] == "unchanged":
            return 0 if unchanged(argv[1], argv[2], argv[3]) else 1
    except InputsError as err:
        print(f"image_inputs: {err}", file=sys.stderr)
        return 2
    except RuntimeError as err:
        print(f"image_inputs: {err}", file=sys.stderr)
        return 1
    print("usage: image_inputs.py candidates KIND SHA | unchanged KIND FROM TO", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
