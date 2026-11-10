"""Shared test setup.

The tests import the plugin's ``modules/`` directly. Compiled bytecode is
not written, so a test run leaves no ``__pycache__`` beside the modules for
``package`` to ship.
"""

import sys

sys.dont_write_bytecode = True
