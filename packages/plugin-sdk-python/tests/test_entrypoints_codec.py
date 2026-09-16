"""Nullable Entrypoints interface fields must not call export_cap(None)."""

from __future__ import annotations

import unittest

from bookclerk_plugin_sdk import _wire as wire


class _Caps:
    def __init__(self) -> None:
        self.slots: list[object] = []

    def export_cap(self, value: object) -> int:
        if value is None:
            raise AssertionError("export_cap must not be called with None")
        idx = len(self.slots)
        self.slots.append(value)
        return idx

    def import_cap(self, index: int | None) -> object | None:
        if index is None:
            return None
        return self.slots[index]


ALL = (
    "eventConsumer",
    "jobRunner",
    "storefront",
    "storage",
    "databaseAdapter",
    "remoteLibrary",
    "cli",
    "oidc",
)


def _round_trip(value: dict) -> dict:
    caps = _Caps()
    data = wire._encode_message(wire._entrypoints_codec, value, caps)
    return wire._decode_message(wire._entrypoints_codec, data, caps)


class EntrypointsNullCapTests(unittest.TestCase):
    def test_no_optional_entrypoints(self) -> None:
        decoded = _round_trip({})
        for name in ALL:
            self.assertNotIn(name, decoded)

    def test_one_entrypoint(self) -> None:
        decoded = _round_trip({"storefront": "sf"})
        self.assertEqual(decoded["storefront"], "sf")
        for name in ALL:
            if name != "storefront":
                self.assertNotIn(name, decoded)

    def test_several_entrypoints(self) -> None:
        decoded = _round_trip(
            {"storefront": "sf", "cli": "cli", "eventConsumer": "ev"}
        )
        self.assertEqual(decoded["storefront"], "sf")
        self.assertEqual(decoded["cli"], "cli")
        self.assertEqual(decoded["eventConsumer"], "ev")
        self.assertNotIn("jobRunner", decoded)

    def test_all_entrypoints(self) -> None:
        payload = {name: name for name in ALL}
        decoded = _round_trip(payload)
        self.assertEqual(decoded, payload)

    def test_event_job_only(self) -> None:
        decoded = _round_trip({"eventConsumer": "ev", "jobRunner": "job"})
        self.assertEqual(decoded["eventConsumer"], "ev")
        self.assertEqual(decoded["jobRunner"], "job")
        self.assertNotIn("storefront", decoded)

    def test_named_entrypoint_only(self) -> None:
        decoded = _round_trip({"cli": "cli-only"})
        self.assertEqual(decoded["cli"], "cli-only")
        self.assertNotIn("storefront", decoded)

    def test_explicit_none_does_not_export(self) -> None:
        decoded = _round_trip({"storefront": None})
        self.assertNotIn("storefront", decoded)


if __name__ == "__main__":
    unittest.main()
