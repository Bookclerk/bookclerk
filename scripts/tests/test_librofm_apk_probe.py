#!/usr/bin/env python3
"""Tests for the Libro.fm APK probe's catalog endpoint and header checks."""

from __future__ import annotations

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path
from types import ModuleType

ROOT = Path(__file__).resolve().parents[2]
PROBE = ROOT / "scripts" / "librofm-apk-probe"
sys.path.insert(0, str(PROBE))


def _load(name: str, path: Path) -> ModuleType:
    """Imports a probe script as a module."""
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


extract = _load("extract_libro_api", PROBE / "extract_libro_api.py")
smoke = _load("live_smoke", PROBE / "live_smoke.py")


def _surface(paths: list[str], headers: list[str]) -> object:
    """An APK surface matching the client constants, with these endpoints and headers."""
    return extract.ApkSurface(
        version_name="7.37.4",
        base_url="https://libro.fm",
        api_prefix="/api/v12/",
        okhttp_user_agent="okhttp/4.12.0",
        headers=headers,
        oauth_token_path="/oauth/token",
        endpoints=[extract.Endpoint(method="GET", path=p, source="Api.java") for p in paths],
    )


CLIENT = extract.ClientConsts(
    default_base_url="https://libro.fm",
    oauth_token_path="/oauth/token",
    library_path="/api/v12/library",
    download_manifest_path="/api/v12/download-manifest",
    packaged_m4b_path="/api/v12/audiobooks/{isbn}/packaged_m4b",
    app_ver="7.37.4",
    user_agent_value="okhttp/4.12.0",
)
ALL_PATHS = [
    "library",
    "download-manifest",
    "audiobooks/{isbn}/packaged_m4b",
    "explore/search",
    "explore/audiobook_details/{isbn}",
]
ALL_HEADERS = ["X-LibroFm-AppVer", "X-LibroFm-Device", "X-LibroFm-OsVer", "Authorization"]


def _fields(drifts: list[dict]) -> set[str]:
    """Names of the blocking drifts."""
    return {d["field"] for d in drifts if d["severity"] == "error"}


class CatalogDriftTests(unittest.TestCase):
    """The probe flags catalog endpoints and headers the plugin relies on."""

    def test_matching_apk_has_no_blocking_drift(self) -> None:
        self.assertEqual(_fields(extract.compare(_surface(ALL_PATHS, ALL_HEADERS), CLIENT)), set())

    def test_renamed_catalog_endpoint_is_blocking(self) -> None:
        paths = [p for p in ALL_PATHS if p != "explore/audiobook_details/{isbn}"]
        paths.append("explore/audiobook/{isbn}")
        drifts = extract.compare(_surface(paths, ALL_HEADERS), CLIENT)
        self.assertEqual(_fields(drifts), {"CATALOG_PATHS"})
        catalog = next(d for d in drifts if d["field"] == "CATALOG_PATHS")
        self.assertEqual(catalog["client"], ["explore/audiobook_details/{isbn}"])
        self.assertTrue(extract.drifts_need_live_smoke(drifts))

    def test_dropped_app_version_header_is_blocking(self) -> None:
        headers = [h for h in ALL_HEADERS if h != "X-LibroFm-AppVer"]
        drifts = extract.compare(_surface(ALL_PATHS, headers), CLIENT)
        self.assertEqual(_fields(drifts), {"APP_HEADERS"})
        self.assertEqual(
            next(d for d in drifts if d["field"] == "APP_HEADERS")["client"], ["X-LibroFm-AppVer"]
        )

    def test_new_header_stays_informational(self) -> None:
        drifts = extract.compare(_surface(ALL_PATHS, ALL_HEADERS + ["X-LibroFm-Api-Key"]), CLIENT)
        self.assertEqual(_fields(drifts), set())
        self.assertIn("headers_not_in_client", {d["field"] for d in drifts})


class CatalogSmokeTests(unittest.TestCase):
    """The public smoke sends what the plugin sends and reports what it maps."""

    def test_catalog_headers_match_the_plugin(self) -> None:
        headers = smoke.catalog_headers("7.37.4", "1.2.3")
        self.assertEqual(
            headers,
            {
                "Accept": "application/json",
                "User-Agent": "bookclerk/1.2.3 (+https://bookclerk.app)",
                "X-LibroFm-AppVer": "7.37.4",
            },
        )
        self.assertNotIn("X-LibroFm-AppVer", smoke.catalog_headers("7.37.4", "1", with_app_ver=False))
        catalog_rs = (
            ROOT / "crates/bookclerk-plugins/optional/source-libro/src/catalog.rs"
        ).read_text(encoding="utf-8")
        # The plugin's user agent and header name, as the smoke builds them.
        self.assertIn('"bookclerk/",', catalog_rs)
        self.assertIn('" (+https://bookclerk.app)"', catalog_rs)
        self.assertIn('.header("x-librofm-appver", APP_VER)', catalog_rs)

    def test_workspace_version_reads_the_package_table(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "Cargo.toml").write_text(
                '[workspace]\nmembers = []\nversion = "9"\n\n[workspace.package]\nedition = "2021"\nversion = "4.5.6"  # bump on release\n',
                encoding="utf-8",
            )
            self.assertEqual(smoke.workspace_version(root), "4.5.6")
            (root / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")
            with self.assertRaises(ValueError):
                smoke.workspace_version(root)
        self.assertTrue(smoke.workspace_version(ROOT))

    def test_details_missing_fields(self) -> None:
        full = {
            "audiobook": {
                "description": "d",
                "publisher": "p",
                "genres": [{"name": "g"}],
                "authors": ["a"],
                "audiobook_info": {"duration": 1, "narrators": ["n"]},
            },
            "purchase_info": {"price": "1.00", "for_sale": False},
            "related_audiobooks": [{}],
        }
        self.assertEqual(smoke.details_missing_fields(full), [])
        self.assertEqual(
            smoke.details_missing_fields({"audiobook": {"publisher": ""}}),
            [
                "audiobook.description",
                "audiobook.publisher",
                "audiobook.genres",
                "audiobook.authors",
                "audiobook.audiobook_info.duration",
                "audiobook.audiobook_info.narrators",
                "purchase_info.price",
                "purchase_info.for_sale",
                "related_audiobooks",
            ],
        )
        self.assertEqual(smoke.details_missing_fields(None), ["data"])


if __name__ == "__main__":
    unittest.main()
