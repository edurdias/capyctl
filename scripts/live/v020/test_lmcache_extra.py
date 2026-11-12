"""LC12/LC17/LC18 CPU protocol tests; these are not native qualification."""
import contextlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

import lmcache_builds
import v020
from rows import lmcache_extra as extra


class CacheExtraTests(unittest.TestCase):
    def test_lc12_is_unified_only_and_rows_are_unique(self):
        catalog=v020.rows()
        for key in ('LC12','LC17','LC18'):
            self.assertEqual(sum(row['id']==key for row in catalog),1)
        row=next(row for row in catalog if row['id']=='LC12')
        self.assertTrue(row['unified'])
        self.assertIsNotNone(v020.skip_reason(row,False,False))

    def test_lc17_configs_include_occupied_room_cases(self):
        ctx=SimpleNamespace()
        with patch.object(extra,'pool_case',return_value=dict(verdict='pass',reason='',evidence=[])) as case:
            result=extra.sizing(ctx,dict(id='LC17'))
        self.assertEqual(result['verdict'],'pass')
        calls=[call.args[2:] for call in case.call_args_list]
        self.assertEqual(len(calls),7)
        self.assertIn(('raised-cap','24GiB',65536,15,False,None,False,False),calls)
        self.assertIn(('room3','4GiB',32768,3,True,None,False,True),calls)
        self.assertIn(('room0','1GiB',32768,0,False,None,False,True),calls)
        self.assertIn(('stated','24GiB',32768,15,False,'15GiB',False,False),calls)
        # T45 / ADR0034 LC17: rounded display GiB is not a valid byte quantity.
        self.assertIn(('auto32','13067437998B',32768,8,False,None,True,False),calls)
        self.assertIn(('derived-cap','13067437998B',65536,12,True,None,False,False),calls)

    def test_lc17_failure_is_not_masked(self):
        with patch.object(extra,'pool_case',return_value=dict(verdict='fail',reason='missing hit',evidence=[])):
            self.assertEqual(extra.sizing(None,dict(id='LC17'))['verdict'],'fail')

    # T45: startup failures without a binding must retain controller diagnostics.
    def test_extra_failure_retains_sanitized_logs(self):
        ctx=SimpleNamespace(save=Mock(),logs=Mock(return_value='sanitized startup error'))
        with patch.object(extra,'sizing',side_effect=AssertionError('isolated role exited before ready')):
            result=extra.run(ctx,dict(id='LC17'))
        self.assertEqual(result['verdict'],'fail')
        ctx.save.assert_called_once_with('failure-logs',dict(logs='sanitized startup error'))

    # T45 / ADR0034 LC18: off has no server and cannot restart-park.
    def test_lc18_uses_restart_only_and_fifteen_full_hits(self):
        records=[]
        ctx=SimpleNamespace(unified=False)
        p=extra.ExtraProtocol(ctx,dict(id='LC18'),'sglang')
        self.assertEqual(p.residency,'restart_only')
        self.assertEqual(p.transfer,'engine_driven')
        p.deploy=Mock(side_effect=['off','on'])
        p.retire=Mock()
        p.lifecycle=Mock()
        p.memory=Mock(return_value={})
        p.server_check=Mock()
        p.footprint=Mock()
        p.record=Mock()
        def request(name,fixture,label,**kwargs):
            records.append((name,label,kwargs))
            return dict(answer=fixture['answer'])
        p.request=request
        labels=['same']+['new-'+str(i) for i in range(1,15,2)]
        extra.endurance(p,dict(fixtures={label:dict(answer=label) for label in labels}))
        hits=[item for item in records if item[2].get('hit')]
        self.assertEqual(len(hits),15)
        self.assertTrue(all(item[0]=='on' for item in hits))
        self.assertEqual([item[2]['expected'] for item in hits],
                         ['same' if i%2==0 else 'new-'+str(i) for i in range(15)])
        self.assertEqual(p.server_check.call_count,31)
        self.assertEqual(p.lifecycle.call_count,30)
        self.assertTrue(all(call.args[1]=='on' for call in p.lifecycle.call_args_list))
        p.footprint.assert_not_called()

    def test_sglang_uses_its_own_offline_version_fingerprint(self):
        ctx=SimpleNamespace(engines={'LMCACHE':'/engine','MODEL_CHAT':'/model'},save=Mock())
        ctx.command=Mock(return_value=SimpleNamespace(stdout=json.dumps(dict(
            fixtures={},versions={'sglang':'0.5.23','lmcache':'0.5.7','transformers':'5'}))))
        ctx.assertion=lambda condition,reason: self.assertTrue(condition,reason)
        extra.fixtures(ctx,[],'sglang')
        script=ctx.command.call_args.args[0][2]
        self.assertIn("('sglang', 'lmcache', 'transformers')",script)
        self.assertNotIn('("vllm", "lmcache", "transformers")',script)

    def test_release_binary_restored_after_variant(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            (root/'bin').mkdir(); (root/'evidence').mkdir()
            binary=root/'bin/capyctl-lc18'; binary.write_bytes(b'qualification binary')
            receipt=dict(qualification_only=True,release_binary_replaced=False,
                         binary_sha256=extra.sha(binary),patch_sha256=extra.sha(extra.HERE/'lmcache-lc18.patch'))
            (root/'evidence/lmcache-lc18-build.json').write_text(json.dumps(receipt))
            original=lambda old=False: root/'bin/capyctl'
            ctx=SimpleNamespace(root=root,engines={},binary=original)
            def assertion(condition,reason):
                if not condition: raise AssertionError(reason)
            ctx.assertion=assertion
            with self.assertRaisesRegex(RuntimeError,'test interruption'):
                with extra.qualification_build(ctx,'LC18'):
                    self.assertEqual(ctx.binary(),binary)
                    raise RuntimeError('test interruption')
            self.assertIs(ctx.binary,original)
            binary.write_bytes(b'changed')
            with self.assertRaisesRegex(AssertionError,'fingerprint mismatch'):
                with extra.qualification_build(ctx,'LC18'): pass

    def test_committed_patches_apply_only_to_source_copies(self):
        repository=Path(__file__).resolve().parents[3]
        with tempfile.TemporaryDirectory() as directory:
            source=Path(directory)/'source'; source.mkdir()
            files=('crates/capyctl-config/src/lmcache.rs','crates/capyctl-agent/src/lmcache_install.rs')
            for name in files:
                destination=source/name; destination.parent.mkdir(parents=True,exist_ok=True)
                shutil.copy2(repository/name,destination)
            before={name:(source/name).read_bytes() for name in files}
            for row in ('lc12','lc18'):
                destination=Path(directory)/row
                receipt=lmcache_builds.prepare(source,destination,extra.HERE/('lmcache-'+row+'.patch'))
                self.assertEqual(len(receipt['patch_sha256']),64)
                text=(destination/files[0]).read_text()
                if row=='lc12':
                    self.assertIn('let _ = residency; TransferMode::LmcacheDriven',text)
                    self.assertIn('gpu: "NVIDIA GB10"',text)
                else:
                    self.assertIn('lc16_passed: true, lc18_passed: true',text)
                self.assertEqual({name:(source/name).read_bytes() for name in files},before)
                with self.assertRaises(ValueError):
                    lmcache_builds.prepare(source,destination,extra.HERE/('lmcache-'+row+'.patch'))
            (source/files[0]).write_text('unrelated source\n')
            with self.assertRaises(subprocess.CalledProcessError):
                lmcache_builds.prepare(source,Path(directory)/'wrong',extra.HERE/'lmcache-lc18.patch')

    def test_finish_cannot_pass_partial_measurement_mode(self):
        ctx=SimpleNamespace(unified=False,functional_only=True,save=Mock())
        p=extra.ExtraProtocol(ctx,dict(id='LC18'),'vllm')
        self.assertEqual(p.finish()['verdict'],'skip')
        p.check(False,'missing GPU charge')
        self.assertEqual(p.finish()['verdict'],'fail')


if __name__=='__main__':
    unittest.main()
