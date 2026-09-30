"""Where capyctl's runtime helpers import from (SPEC §9.1 / T21, T37).

The entries run under `python -I -S`, which leaves the script's directory off
sys.path. They must then import their siblings from the runtime directory the
host agent verified, and nothing else: the directory above it is not checked,
so it must never become an import root (a module there would shadow the
standard library or a sibling). CPU-only; never evidence a build serves.
"""

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

RUNTIME = Path(__file__).resolve().parents[1]

PROBE = r"""
import os, runpy, sys
entry = sys.argv[1]
sys.argv = [entry]
try:
    runpy.run_path(entry, run_name="__main__")
except SystemExit:
    pass
runtime_dir = os.path.dirname(entry)
parent = os.path.dirname(runtime_dir)
print("PARENT_ON_PATH" if parent in sys.path else "PARENT_OFF_PATH")
package = sys.modules.get("runtime")
print("PACKAGE", list(getattr(package, "__path__", [])) == [runtime_dir])
"""


class ImportRootTests(unittest.TestCase):
    # T21 T37
    def test_entries_import_siblings_from_the_runtime_directory_only(self):
        with tempfile.TemporaryDirectory() as root:
            parent = Path(root)
            runtime = parent / "runtime"
            runtime.mkdir()
            for module in RUNTIME.glob("*.py"):
                shutil.copy(module, runtime / module.name)
            # A module in the unverified parent that would shadow a sibling.
            shadow = parent / "runtime.py"
            shadow.write_text("raise SystemExit('parent shadow imported')\n")
            for entry in ("sglang_entry.py", "engine_capabilities.py"):
                result = subprocess.run(
                    [sys.executable, "-I", "-S", "-B", "-c", PROBE, str(runtime / entry)],
                    capture_output=True, text=True, timeout=60)
                self.assertIn("PARENT_OFF_PATH", result.stdout, entry + result.stderr)
                self.assertIn("PACKAGE True", result.stdout, entry + result.stderr)
                self.assertNotIn("parent shadow", result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
