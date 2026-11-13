"""CPU setup dependency tests; not native qualification."""
import unittest
from setup_selection import dependencies
from lmcache_builds import selected_builds


class SetupSelectionTests(unittest.TestCase):
    def test_held_out_rows_have_no_dependencies(self):
        self.assertEqual(dependencies('TR1,TR3,P3-TENSORRT_LLM_121'), set())

    def test_lc12_is_not_built_or_installed_on_discrete(self):
        self.assertEqual(dependencies('LC12'), set())
        self.assertEqual(selected_builds('LC12', False), [])
        self.assertEqual(selected_builds('LC12', True), ['lc12'])

    def test_lc18_only_builds_lc18(self):
        self.assertEqual(selected_builds('LC18', False), ['lc18'])
        self.assertNotIn('CAPYCTL_LC12', dependencies('LC18'))
        self.assertIn('CAPYCTL_LC18', dependencies('LC18'))

    def test_engine_selection_does_not_install_unrelated_dependencies(self):
        self.assertEqual(dependencies('P3-vllm033'), {'VLLM_033', 'MODEL_CHAT'})
        self.assertEqual(dependencies('TF1'), {'TENSORFOLD_126'})
        self.assertNotIn('LMCACHE', dependencies('EM6'))
        self.assertIn('EM6_VLLM', dependencies('EM6'))

    def test_preservation_rows_include_base_and_cache(self):
        for row in ('LC13', 'LC14', 'LC15', 'LC16'):
            with self.subTest(row=row):
                needed = dependencies(row)
                self.assertTrue({'VLLM_033', 'LMCACHE', 'VLLM_034',
                                 'LMCACHE_VLLM_034', 'SGLANG_0523',
                                 'LMCACHE_SGLANG', 'MODEL_CHAT'} <= needed)
                self.assertNotIn('CAPYCTL_LC18', needed)
                self.assertTrue({'VLLM_034', 'LMCACHE_VLLM_034'} <=
                                dependencies(row + 'U', unified=True))

    def test_full_discrete_catalog_does_not_install_unified_or_trt(self):
        needed = dependencies()
        self.assertNotIn('TENSORFOLD_100', needed)
        self.assertNotIn('CAPYCTL_LC12', needed)
        self.assertFalse(any('TENSORRT' in key for key in needed))
        self.assertTrue({'VLLM_034', 'LMCACHE_VLLM_034'} <= needed)

    def test_other_cache_rows_do_not_install_preserving_vllm(self):
        for row in ('LC17', 'LC18'):
            self.assertNotIn('LMCACHE_VLLM_034', dependencies(row))

    def test_skipped_selection_never_bootstraps_tools(self):
        import subprocess
        import tempfile
        from pathlib import Path
        setup = Path(__file__).with_name('setup.sh')
        script = '''
source "$1"
export HOME=$2 V020=$2/session BIN=$2/session/bin EVIDENCE=$2/session/evidence
ONLY=$3
mkdir -p "$V020"
uname() { printf 'x86_64\\n'; }
v020_setup_tools() { exit 99; }
setup
'''
        for selection in ('TR1,P3-TENSORRT_LLM_121', 'LC12'):
            with self.subTest(selection=selection), tempfile.TemporaryDirectory() as root:
                result = subprocess.run(['bash', '-c', script, 'setup', str(setup), root, selection],
                                        capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertFalse((Path(root) / 'session/tools').exists())
