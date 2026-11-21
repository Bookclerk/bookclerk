"""``env.NETWORK``: the network grant a guest sees at ``open``."""

from __future__ import annotations

import unittest


class NetworkBindingTest(unittest.TestCase):
    """The bridge context's ``network`` reaches the author as ``env.NETWORK``."""

    def test_network_grant_reaches_env(self) -> None:
        from bookclerk_plugin_sdk.workerd import _invocation_env

        env = _invocation_env(
            {},
            {
                "network": {
                    "outbound": True,
                    "domains": ["api.example.com"],
                    "settings": [
                        {
                            "key": "server_url",
                            "network": "fetch",
                            "destination": "http://abs.home.lan:13378",
                            "state": "pending",
                        }
                    ],
                }
            },
        )
        self.assertTrue(env.NETWORK["outbound"])
        self.assertEqual(env.NETWORK["domains"], ["api.example.com"])
        self.assertEqual(env.NETWORK["tcp"], [])
        self.assertEqual(env.NETWORK["settings"][0]["state"], "pending")

    def test_no_network_in_context_means_no_binding(self) -> None:
        from bookclerk_plugin_sdk.workerd import _invocation_env

        env = _invocation_env({}, {})
        self.assertIsNone(env.get("NETWORK"))


if __name__ == "__main__":
    unittest.main()
