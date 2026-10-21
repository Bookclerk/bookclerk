#!/usr/bin/env python3
"""APK probe extractors and fixer for GraphicAudio and Chirp (offline fixtures)."""

from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PROBES = ROOT / "scripts" / "source-probes"
sys.path.insert(0, str(PROBES))


def _load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


extract = _load("extract_apk_api", PROBES / "extract_apk_api.py")
apply_mod = _load("apply_apk_updates", PROBES / "apply_apk_updates.py")

OKHTTP_UTIL = 'public final class Util {\n    public static final String userAgent = "okhttp/{ver}";\n}\n'

GA_CLIENT = {
    "DEFAULT_BASE_URL": "https://www.graphicaudio.net/access",
    "LOGIN_PATH": "/activation/login",
    "REMOVE_PATH": "/activation/remove",
    "PRODUCTS_PATH": "/api/products",
    "LINKS_PATH": "/api/links",
    "USER_AGENT_VALUE": "okhttp/4.12.0 GraphicAudio/Bookclerk",
}


def _tree(root: Path, files: dict[str, str]) -> Path:
    for rel, text in files.items():
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
    return root


def _ga_sources(root: Path, *, base: str, login: str, products: str, okhttp: str) -> Path:
    api = (
        "public interface GaApi {\n"
        f'    @POST("{login}")\n    Call<Login> login();\n'
        '    @POST("activation/remove")\n    Call<Void> remove();\n'
        f'    @GET("{products}")\n    Call<List<P>> products();\n'
        '    @GET("api/links")\n    Call<Links> links(@Query("product") String id);\n'
        "}\n"
    )
    module = (
        "public final class NetModule {\n"
        f'    Retrofit r = new Retrofit.Builder().baseUrl("{base}").build();\n'
        '    String store = "https://www.graphicaudio.net";\n'
        "}\n"
    )
    return _tree(
        root,
        {
            "com/efsharp/graphicaudio/GaApi.java": api,
            "com/efsharp/graphicaudio/NetModule.java": module,
            "okhttp3/internal/Util.java": OKHTTP_UTIL.replace("{ver}", okhttp),
        },
    )


class GraphicAudioExtract(unittest.TestCase):
    def test_no_drift_when_apk_matches(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            src = _ga_sources(
                Path(tmp),
                base="https://www.graphicaudio.net/access/",
                login="activation/login",
                products="api/products",
                okhttp="4.12.0",
            )
            ex = extract.extract_graphicaudio(src, GA_CLIENT)
            drifts = extract.compare(extract.SPECS["graphicaudio"], GA_CLIENT, ex)
            self.assertEqual([d for d in drifts if d.severity == "error"], [])

    def test_moved_paths_and_okhttp_become_suggestions(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            src = _ga_sources(
                Path(tmp),
                base="https://www.graphicaudio.net/access/",
                login="v2/activation/login",
                products="api/v2/products",
                okhttp="5.0.1",
            )
            ex = extract.extract_graphicaudio(src, GA_CLIENT)
            drifts = extract.compare(extract.SPECS["graphicaudio"], GA_CLIENT, ex)
            got = {d.field: d.apk for d in drifts if d.severity == "error"}
            self.assertEqual(
                got,
                {
                    "LOGIN_PATH": "/v2/activation/login",
                    "PRODUCTS_PATH": "/api/v2/products",
                    "USER_AGENT_VALUE": "okhttp/5.0.1 GraphicAudio/Bookclerk",
                },
            )

    def test_ambiguous_endpoint_is_unresolved(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            src = _ga_sources(
                Path(tmp),
                base="https://www.graphicaudio.net/access/",
                login="v2/activation/login",
                products="api/products",
                okhttp="4.12.0",
            )
            extra = src / "com/efsharp/graphicaudio/OtherApi.java"
            extra.write_text('interface O {\n    @POST("thirdparty/login")\n    Call<X> x();\n}\n', encoding="utf-8")
            ex = extract.extract_graphicaudio(src, GA_CLIENT)
            self.assertIn("LOGIN_PATH", ex.unresolved)
            drifts = extract.compare(extract.SPECS["graphicaudio"], GA_CLIENT, ex)
            login = [d for d in drifts if d.field == "LOGIN_PATH"]
            self.assertEqual(len(login), 1)
            self.assertIsNone(login[0].apk)

    def test_moved_base_url_is_suggested(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            src = _ga_sources(
                Path(tmp),
                base="https://api.graphicaudio.net/access/",
                login="activation/login",
                products="api/products",
                okhttp="4.12.0",
            )
            ex = extract.extract_graphicaudio(src, GA_CLIENT)
            self.assertEqual(ex.values.get("DEFAULT_BASE_URL"), "https://api.graphicaudio.net/access")


class ChirpExtract(unittest.TestCase):
    CLIENT = {
        "DEFAULT_GRAPHQL_URL": "https://api.chirpbooks.com/api/graphql",
        "USER_AGENT_VALUE": "okhttp/4.12.0 Chirp/Bookclerk",
    }

    def _sources(self, root: Path, api_root: str) -> Path:
        return _tree(
            root,
            {
                "com/chirpbooks/chirp/BuildConfig.java": (
                    "public final class BuildConfig {\n"
                    f'    public static final String MOCKINGJAY_API_ROOT = "{api_root}";\n'
                    "}\n"
                ),
                "com/mockingjay/AndroidCurrentUserAudiobooksQuery.java": (
                    'class Q {\n    public static final String OPERATION_NAME = "AndroidCurrentUserAudiobooks";\n}\n'
                ),
                "okhttp3/internal/Util.java": OKHTTP_UTIL.replace("{ver}", "4.12.0"),
            },
        )

    def test_api_root_from_build_config(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            src = self._sources(Path(tmp), "https://api.chirpbooks.com/api")
            ex = extract.extract_chirp(src, self.CLIENT)
            drifts = extract.compare(extract.SPECS["chirp"], self.CLIENT, ex)
            self.assertEqual([d for d in drifts if d.severity == "error"], [])
            self.assertIn("AndroidCurrentUserAudiobooks", ex.facts["apk_operations"])

    def test_moved_api_root_is_suggested(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            src = self._sources(Path(tmp), "https://api2.chirpbooks.com/api/")
            ex = extract.extract_chirp(src, self.CLIENT)
            self.assertEqual(ex.values["DEFAULT_GRAPHQL_URL"], "https://api2.chirpbooks.com/api/graphql")


class ApplyUpdates(unittest.TestCase):
    def test_rewrite_const_keeps_visibility_and_doc(self) -> None:
        text = '/// Doc.\npub const LOGIN_PATH: &str = "/activation/login";\nconst USER_AGENT_VALUE: &str = "okhttp/4.12.0 X";\n'
        text, old = apply_mod.rewrite_const(text, "LOGIN_PATH", "/v2/activation/login")
        self.assertEqual(old, "/activation/login")
        text, old = apply_mod.rewrite_const(text, "USER_AGENT_VALUE", "okhttp/5.0.1 X")
        self.assertEqual(old, "okhttp/4.12.0 X")
        self.assertEqual(
            text,
            '/// Doc.\npub const LOGIN_PATH: &str = "/v2/activation/login";\nconst USER_AGENT_VALUE: &str = "okhttp/5.0.1 X";\n',
        )

    def test_missing_const_reports_none(self) -> None:
        _, old = apply_mod.rewrite_const("const A: &str = \"x\";\n", "B", "y")
        self.assertIsNone(old)

    def test_main_rejects_untracked_and_unsafe_values(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report = Path(tmp) / "report.json"
            report.write_text(json.dumps({"suggested": {"CLIENT_SECRET": "x"}}), encoding="utf-8")
            self.assertEqual(apply_mod.main(["--source", "chirp", "--report", str(report)]), 2)
            report.write_text(json.dumps({"suggested": {"USER_AGENT_VALUE": 'a"b'}}), encoding="utf-8")
            self.assertEqual(apply_mod.main(["--source", "chirp", "--report", str(report)]), 2)


if __name__ == "__main__":
    unittest.main()
