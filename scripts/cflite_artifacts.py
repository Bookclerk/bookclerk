#!/usr/bin/env python3
"""Bounded corpus and coverage lookups for ClusterFuzzLite.

ClusterFuzzLite keeps each target's corpus and the coverage report as Actions
artifacts, and finds one by listing every artifact in the repository page by
page until a name matches. Its pager stops only once it has seen as many
artifacts as the first page's ``total_count``. When artifacts expire or are
deleted while it pages, or no artifact has the name (a target added since
the last batch run), it keeps asking for empty pages. On 2026-11-05 one pull
request run asked for 4,840 pages for the new ``cover_wire`` target and used
up the repository's ``GITHUB_TOKEN`` budget, so every other workflow's API
calls failed for the rest of the hour.

This module replaces that search, inside the pinned run-fuzzers image, with
one request: ``GET /repos/{owner}/{repo}/actions/artifacts?name=<name>``,
which lists the matching artifacts newest first. The full listing is
disabled, so no other path can page through every artifact. See
``docs/contributing/fuzzing.md#artifact-lookups``.

``newest_artifact`` is a sans-IO function over the response body, so
``scripts/tests/test_cflite_artifacts.py`` drives the same code the image
runs. ``install`` patches ClusterFuzzLite's filestore and fails closed when
the image no longer has the method it replaces.

Usage (inside the run-fuzzers image, as the pull-request action's
entrypoint): ``python3 scripts/cflite_artifacts.py``. It reads the same
environment as the image's own entrypoint and exits with its code.
``scripts/fuzz-crash-triage.py run`` calls ``install`` for scheduled runs.
"""

from __future__ import annotations

import json
import logging
import os
import sys
import urllib.parse
from typing import Any

# Where the run-fuzzers image keeps ClusterFuzzLite's Python sources.
CIFUZZ_DIR = "/opt/oss-fuzz/infra/cifuzz"

# Matching artifacts read per lookup; the newest unexpired one is used. The
# listing is newest first, so the newest is always on this one page.
PAGE_SIZE = 100

# How long one lookup may take before it fails, in seconds.
REQUEST_TIMEOUT_SECONDS = 60

# Used when the runner does not set `GITHUB_API_URL`.
DEFAULT_API_URL = "https://api.github.com"


def listing_url(api_url: str, owner: str, repo: str) -> str:
    """Returns the repository's artifact listing endpoint.

    Args:
        api_url: The REST API root, such as ``https://api.github.com``; a
            trailing slash is ignored.
        owner: The repository owner.
        repo: The repository name.

    Returns:
        ``<api>/repos/<owner>/<repo>/actions/artifacts``, with the owner and
        name percent-encoded.
    """
    root = api_url.rstrip("/")
    return f"{root}/repos/{urllib.parse.quote(owner, safe='')}/{urllib.parse.quote(repo, safe='')}/actions/artifacts"


def newest_artifact(raw: bytes, name: str, listing: str) -> dict[str, Any] | None:
    """Picks the artifact ClusterFuzzLite should download.

    Args:
        raw: The JSON body of ``GET <listing>?name=<name>``, newest first.
        name: The artifact name searched for, such as
            ``cifuzz-corpus-mp4_parse``.
        listing: The listing endpoint from ``listing_url``. An artifact's
            download URL must be ``<listing>/<id>/zip``, so the token
            ClusterFuzzLite sends with the download never leaves this
            repository's API.

    Returns:
        The first entry whose ``name`` is ``name``, whose ``expired`` is
        ``false``, whose ``id`` is a positive integer and whose
        ``archive_download_url`` is its download URL; ``None`` when no entry
        qualifies or the body is not the expected shape.
    """
    try:
        doc = json.loads(raw.decode("utf-8"))
    except ValueError:
        return None
    if not isinstance(doc, dict):
        return None
    artifacts = doc.get("artifacts")
    if not isinstance(artifacts, list):
        return None
    for entry in artifacts:
        if not isinstance(entry, dict):
            continue
        ident = entry.get("id")
        if type(ident) is not int or ident < 1:
            continue
        if entry.get("name") != name or entry.get("expired") is not False:
            continue
        if entry.get("archive_download_url") != f"{listing}/{ident}/zip":
            continue
        return entry
    return None


def install(gha: Any, filestore: Any, github_api: Any) -> None:
    """Makes ClusterFuzzLite's filestore look artifacts up by name.

    Args:
        gha: The image's ``filestore.github_actions`` module.
        filestore: The image's ``filestore`` module (for ``FilestoreError``).
        github_api: The image's ``filestore.github_actions.github_api``
            module (for ``_do_get_request``).

    Raises:
        RuntimeError: The image no longer has the methods this replaces, so
            it would still page through every artifact.
    """
    store = getattr(gha, "GithubActionsFilestore", None)
    error = getattr(filestore, "FilestoreError", None)
    get = getattr(github_api, "_do_get_request", None)
    if (
        store is None
        or not callable(getattr(store, "_find_artifact", None))
        or not callable(getattr(store, "_list_artifacts", None))
        or not isinstance(error, type)
        or not issubclass(error, Exception)
        or not callable(get)
    ):
        raise RuntimeError("ClusterFuzzLite's artifact lookup not found; refusing to page through every artifact")

    def find_artifact(self: Any, name: str) -> dict[str, Any] | None:
        """Returns the newest unexpired artifact called ``name``, or ``None``.

        One request, whatever the repository holds.
        """
        listing = listing_url(
            os.environ.get("GITHUB_API_URL") or DEFAULT_API_URL,
            self.config.project_repo_owner,
            self.config.project_repo_name,
        )
        response = get(
            listing,
            params={"name": name, "per_page": str(PAGE_SIZE)},
            headers=self.github_api_http_headers,
            timeout=REQUEST_TIMEOUT_SECONDS,
        )
        if response.status_code != 200:
            logging.error("Artifact lookup for %s failed: HTTP %d.", name, response.status_code)
            raise error("Github API request failed.")
        artifact = newest_artifact(response.content, name, listing)
        logging.debug("Artifact: %s.", artifact)
        return artifact

    def list_artifacts(self: Any) -> Any:
        """Refuses the full listing, which pages through every artifact."""
        del self
        raise error("listing every artifact is disabled; look artifacts up by name")

    store._find_artifact = find_artifact
    store._list_artifacts = list_artifacts


def install_in_image() -> None:
    """Imports ClusterFuzzLite from the run-fuzzers image and calls ``install``.

    Raises:
        ImportError: Not running inside the run-fuzzers image.
        RuntimeError: As ``install``.
    """
    if CIFUZZ_DIR not in sys.path:
        sys.path.insert(0, CIFUZZ_DIR)
    import filestore  # type: ignore[import-not-found]  # noqa: PLC0415
    import filestore.github_actions as gha  # type: ignore[import-not-found]  # noqa: PLC0415
    from filestore.github_actions import github_api  # type: ignore[import-not-found]  # noqa: PLC0415

    install(gha, filestore, github_api)


def main() -> int:
    """Runs the image's own ``run_fuzzers`` entrypoint with bounded lookups.

    Returns:
        The entrypoint's exit code, or 1 when the lookup could not be
        installed.
    """
    try:
        install_in_image()
    except (ImportError, RuntimeError) as err:
        print(f"::error::{err}", flush=True)
        return 1
    import run_fuzzers_entrypoint  # type: ignore[import-not-found]  # noqa: PLC0415

    return int(run_fuzzers_entrypoint.main())


if __name__ == "__main__":
    sys.exit(main())
