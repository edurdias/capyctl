#!/usr/bin/env python3
"""CPU-only regression checks; not native-engine qualification."""
import argparse
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sqlite3
import textwrap
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("mg", HERE / "mg.py")
mg = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mg)


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        scratch = Path.home() / ".cache/capyctl-agent-tmp/mg-tests"
        scratch.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.tmp = tempfile.TemporaryDirectory(dir=scratch)
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.raw = self.root / "raw"
        self.raw.mkdir()
        (self.raw / "versions.txt").write_text("approved engine fingerprints\n")
        for name in ("events-s1.jsonl", "journal-s1.jsonl"):
            (self.raw / name).write_text("{}\n")
        self.settings = self.root / "settings.json"
        self.settings.write_text(json.dumps({"commit": "abc", "only": "all", "cycles": 5,
                                             "hb_cycles": 3, "stall": 30, "harness": "sha"}))

    def verdict(self, lines, only="all"):
        (self.raw / "results.tsv").write_text(lines)
        args = argparse.Namespace(results=str(self.raw / "results.tsv"), out=str(self.root / "verdict.md"),
                                  settings=str(self.settings), only=only, attempts=None)
        with contextlib.redirect_stdout(io.StringIO()):
            rc = mg.cmd_verdict(args)
        return rc, json.loads((self.root / "verdict.json").read_text())

    def test_missing_rows_never_pass(self):
        rc, report = self.verdict("")
        self.assertNotEqual(rc, 0)
        self.assertFalse(report["qualified"])
        self.assertEqual(len(report["rows"]), 15)

    def test_one_passing_part_does_not_complete_row(self):
        rc, report = self.verdict("MG1\tready\tpass\tReady\n", "MG1")
        self.assertNotEqual(rc, 0)
        self.assertIn("charge", report["rows"]["MG1"]["missing_parts"])

    def test_missing_refusal_evidence_is_failure(self):
        rc, report = self.verdict("MG12\ttp3\tpass\trefused\nMG12\tpin\tpass\trefused\n", "MG12")
        self.assertNotEqual(rc, 0)
        self.assertIn("MG12-tp3.out", report["rows"]["MG12"]["missing_evidence"])

    def test_startup_peaks_require_host_and_representative_device_measurements(self):
        records = [{"domain": domain, "tensor_parallel": 2, "peak_bytes": 1024}
                   for domain in ("system", "gpu1")]
        startup = {"measured": records, "host_provenance": "measured", "device_provenance": "measured"}
        self.assertTrue(mg.startup_peaks_complete(startup))
        self.assertTrue(mg.startup_peaks_complete(dict(startup, measured=[
            dict(record, domain="gpu0") if record["domain"] == "gpu1" else record for record in records])))
        for index in range(len(records)):
            with self.subTest(missing=records[index]["domain"]):
                self.assertFalse(mg.startup_peaks_complete(dict(
                    startup, measured=records[:index] + records[index + 1:])))
            for value in (0, -1, None, "1024", True):
                with self.subTest(domain=records[index]["domain"], peak_bytes=value):
                    changed = [dict(record) for record in records]
                    changed[index]["peak_bytes"] = value
                    self.assertFalse(mg.startup_peaks_complete(dict(startup, measured=changed)))
        self.assertFalse(mg.startup_peaks_complete(dict(startup, measured=[
            dict(record, tensor_parallel=1) for record in records])))
        for key in ("host_provenance", "device_provenance"):
            self.assertFalse(mg.startup_peaks_complete(dict(startup, **{key: "derived"})))
        for invalid in ({}, {"measured": []}, {"measured": None}, {"bytes": 1024}, None):
            self.assertFalse(mg.startup_peaks_complete(invalid))

    def test_enrolled_requires_full_remote_mg1_parts_and_artifacts(self):
        parts = ("remote", "charge", "listeners", "peaks", "old")
        files = ("MG11-status.json", "MG11-gpus.json", "MG11-charge.json",
                 "MG11-listeners.json", "MG11-peaks.json", "MG11-owned.jsonl",
                 "MG11-deployments.tsv",
                 "start-mg11-new.out", "mg11-new-deploy.out", "chat-mg11.json",
                 "mg11-old-deploy.out", "mg11-old-start.out",
                 "events-srv.jsonl", "journal-srv.jsonl")
        for name in files:
            (self.raw / name).write_text("{}\n" if name.endswith((".json", ".jsonl")) else "evidence\n")
        logs = self.raw / "engine-logs/h1"
        logs.mkdir(parents=True)
        (logs / "engine.log").write_text("engine startup evidence\n")
        lines = "".join(f"MG11\t{part}\tpass\tevidence\n" for part in parts)
        rc, report = self.verdict(lines, "MG11")
        self.assertEqual(rc, 0)
        self.assertTrue(report["selected_pass"])
        self.assertFalse(report["qualified"])
        for part in parts:
            with self.subTest(part=part):
                rc, report = self.verdict(lines.replace(f"MG11\t{part}\tpass\tevidence\n", ""), "MG11")
                self.assertNotEqual(rc, 0)
                self.assertIn(part, report["rows"]["MG11"]["missing_parts"])
        for name in files:
            with self.subTest(file=name):
                (self.raw / name).unlink()
                rc, report = self.verdict(lines, "MG11")
                self.assertNotEqual(rc, 0)
                self.assertIn(name, report["rows"]["MG11"]["missing_evidence"])
                (self.raw / name).write_text("{}\n" if name.endswith((".json", ".jsonl")) else "evidence\n")

    def test_selected_success_is_not_full_qualification(self):
        for part in ("tp3", "pin"):
            (self.raw / f"MG12-{part}.out").write_text("refused\n")
        rc, report = self.verdict("MG12\ttp3\tpass\trefused\nMG12\tpin\tpass\trefused\n", "MG12")
        self.assertEqual(rc, 0)
        self.assertFalse(report["qualified"])
        self.assertTrue(report["selected_pass"])

    def test_skip_or_earlier_failure_cannot_be_hidden_by_pass(self):
        for part in ("tp3", "pin"):
            (self.raw / f"MG12-{part}.out").write_text("refused\n")
        for result in ("skip", "fail"):
            rc, report = self.verdict(f"MG12\ttp3\t{result}\tfirst\nMG12\ttp3\tpass\tsecond\nMG12\tpin\tpass\tok\n", "MG12")
            self.assertNotEqual(rc, 0)
            self.assertEqual(report["rows"]["MG12"]["result"], "fail")

    def test_malformed_result_is_failure(self):
        rc, report = self.verdict("MG12\ttp3\tpass\n", "MG12")
        self.assertNotEqual(rc, 0)
        self.assertTrue(report["errors"])

    def test_fit_needs_positive_model_buffers_on_both_cards_within_grants(self):
        charge = {"gpu0": [{"bytes": 2 * 2**20}], "gpu1": [{"bytes": 2 * 2**20}]}
        self.assertFalse(mg.fit_matches("gpu0=1MiB gpu1=1MiB", charge))
        self.assertFalse(mg.fit_matches("CUDA0 model buffer size = 1.00 MiB", charge))
        text = "CUDA0 model buffer size = 1.00 MiB\nCUDA1 model buffer size = 1.50 MiB"
        self.assertTrue(mg.fit_matches(text, charge))
        self.assertFalse(mg.fit_matches(text.replace("1.50", "3.00"), charge))
        self.assertFalse(mg.fit_matches(text.replace("1.50", "0.00"), charge))

    def test_redaction_keeps_evidence_not_credentials(self):
        text = 'api_key="private-value" Authorization: Bearer hidden-token hf_ABC123\n/root/mg CUDA0 model buffer size = 1.00 MiB'
        clean = mg.sanitize(text, ["private-value"], "203.0.113.9")
        for secret in ("private-value", "hidden-token", "hf_ABC123", "/root/"):
            self.assertNotIn(secret, clean)
        self.assertIn("CUDA0 model buffer size = 1.00 MiB", clean)

    def make_database(self, path):
        path.parent.mkdir(parents=True, exist_ok=True)
        with contextlib.closing(sqlite3.connect(path)) as db:
            db.execute("CREATE TABLE management_events(sequence, recorded_at_ms, kind, deployment_id, payload_json)")
            db.execute("CREATE TABLE journal_entries(recorded_at, state, evidence)")
            db.execute("INSERT INTO journal_entries VALUES(1, 'ready', 'owned launch')")
            db.commit()

    def test_enrolled_snapshot_exports_journal_and_refuses_missing_database(self):
        root = self.root / "host"
        database = root / "attempts/a/state/srv/srv.sqlite3"
        self.make_database(database)
        args = argparse.Namespace(root=str(root), attempt="a", host="")
        self.assertEqual(mg.cmd_snapshot(args), 0)
        exported = root / "evidence/a/raw/journal-srv.jsonl"
        self.assertIn("owned launch", exported.read_text())
        database.unlink()
        self.assertNotEqual(mg.cmd_snapshot(args), 0)

    def test_snapshot_redaction_preserves_json_and_nested_journal_evidence(self):
        root = self.root / "host"
        database = root / "attempts/a/state/srv/srv.sqlite3"
        self.make_database(database)
        evidence = "server_args={'api_key': None, 'ssl_keyfile_password': 'private-password'}"
        payload = json.dumps({"api_key": "private-key", "detail": evidence})
        with contextlib.closing(sqlite3.connect(database)) as db:
            db.execute("UPDATE journal_entries SET evidence=?", (evidence,))
            db.execute("INSERT INTO management_events VALUES(1, 1, 'failed', 'model', ?)", (payload,))
            db.commit()
        args = argparse.Namespace(root=str(root), attempt="a", host="")
        self.assertEqual(mg.cmd_snapshot(args), 0)
        exported = root / "evidence/a/raw"
        journal = json.loads((exported / "journal-srv.jsonl").read_text())
        events = json.loads((exported / "events-srv.jsonl").read_text())
        nested = json.loads(events["payload_json"])
        self.assertIn("server_args=", journal["evidence"])
        self.assertEqual(nested["api_key"], "<REDACTED>")
        for name in ("journal-srv.jsonl", "events-srv.jsonl"):
            text = (exported / name).read_text()
            self.assertNotIn("private-password", text)
            self.assertNotIn("private-key", text)
            # The second local sanitization pass must preserve the export too.
            self.assertEqual(json.loads(mg.sanitize(text)), json.loads(text))

    def test_snapshot_excludes_credentials_and_database_and_copies_live_logs(self):
        root = self.root / "host"
        attempt = root / "attempts/a"
        raw = attempt / "raw"
        raw.mkdir(parents=True)
        (raw / "results.tsv").write_text("MG3\tfit\tfail\tmissing log\n")
        (raw / "private.sqlite3").write_bytes(b"private-state-secret")
        logs = attempt / "state/s1/logs"
        logs.mkdir(parents=True)
        (logs / "engine.log").write_text('api_key="secret-value"\nCUDA0 model buffer size = 1 MiB\n')
        (logs / "engine.log.1").write_text("CUDA1 model buffer size = 1 MiB\n")
        identity = attempt / "state/s1/identity"
        identity.mkdir()
        (identity / "credentials").write_text("inference_key: secret-value\n")
        (attempt / "settings.json").write_text('{"commit":"abc"}')
        self.make_database(attempt / "state/s1/server/srv.sqlite3")
        args = argparse.Namespace(root=str(root), attempt="a", host="203.0.113.9")
        self.assertEqual(mg.cmd_snapshot(args), 0)
        dest = root / "evidence/a"
        self.assertTrue((dest / "raw/engine-logs/s1/engine.log").is_file())
        self.assertIn("CUDA1 model buffer", (dest / "raw/engine-logs/s1/engine.log.1").read_text())
        self.assertFalse(list(dest.rglob("credentials")))
        self.assertFalse(list(dest.rglob("*.sqlite3")))
        self.assertNotIn("secret-value", (dest / "raw/engine-logs/s1/engine.log").read_text())

    def test_rank_exit_names_target_without_claiming_order_or_cause(self):
        events = self.root / "events.jsonl"
        args = argparse.Namespace(events=str(events), rank=1)
        for ranks in ("rank 0 on gpu0; rank 1 on gpu1", "rank 1 on gpu1; rank 0 on gpu0"):
            events.write_text(json.dumps({"kind": "engine_rank_exited", "evidence": ranks}) + "\n")
            self.assertEqual(mg.cmd_rank_exit(args), 0)
        events.write_text(json.dumps({"kind": "engine_rank_exited", "evidence": "rank 0 on gpu0; rank 10 on gpu10"}) + "\n")
        self.assertNotEqual(mg.cmd_rank_exit(args), 0)

    def test_enrolled_and_standalone_chat_use_their_credential_formats(self):
        credentials = self.root / "credentials"
        args = argparse.Namespace(no_key=False, cred=str(credentials))
        output = io.StringIO()
        for text in ('api_key: test-key\nadmin_token: admin-key\n',
                     '{"version":1,"api_key":"test-key","admin_token":"admin-key"}'):
            credentials.write_text(text)
            with contextlib.redirect_stdout(output):
                headers = mg.headers_for(args)
            self.assertEqual(headers["Authorization"], "Bearer test-key")
        self.assertEqual(output.getvalue(), "")

    def test_vllm_comparison_uses_actual_argv_and_drops_keys(self):
        log = self.root / "engine.log"
        log.write_text("non-default args: {'tensor_parallel_size': 99}\n")
        argv = self.root / "argv.txt"
        argv.write_text("vllm\nserve\n/model\n--tensor-parallel-size\n2\n--port\n8740\n--api-key\nprivate-value\n--capyctl-user-args\n--max-model-len\n32768\n")
        args = argparse.Namespace(log=str(log), engine="vllm", argv=str(argv), pid=None)
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            self.assertEqual(mg.cmd_engine_args(args), 0)
        self.assertEqual(json.loads(output.getvalue()), {"max_model_len": 32768, "tensor_parallel_size": 2})

    def test_failed_rerun_does_not_resurrect_earlier_pass(self):
        attempts = self.root / "attempts"
        prior = attempts / "001"
        (prior / "raw").mkdir(parents=True)
        settings = json.loads(self.settings.read_text())
        settings["only"] = "MG12"
        (prior / "settings.json").write_text(json.dumps(settings))
        (prior / "raw/versions.txt").write_text((self.raw / "versions.txt").read_text())
        (prior / "raw/results.tsv").write_text("MG12\ttp3\tpass\tok\nMG12\tpin\tpass\tok\n")
        for part in ("tp3", "pin"):
            (prior / f"raw/MG12-{part}.out").write_text("refused\n")
        current = attempts / "002"
        (current / "raw").mkdir(parents=True)
        (current / "settings.json").write_text(json.dumps(settings))
        (current / "raw/versions.txt").write_text((self.raw / "versions.txt").read_text())
        (current / "raw/results.tsv").write_text("")
        args = argparse.Namespace(results=str(current / "raw/results.tsv"), out=str(current / "verdict.md"),
                                  settings=str(current / "settings.json"), only="MG12", attempts=str(attempts))
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertNotEqual(mg.cmd_verdict(args), 0)
        report = json.loads((current / "verdict.json").read_text())
        self.assertEqual(report["rows"]["MG12"]["attempt"], "002")
        self.assertEqual(report["rows"]["MG12"]["result"], "fail")

    def test_incomplete_historical_rerun_owns_row_instead_of_old_pass(self):
        attempts = self.root / "attempts"
        settings = json.loads(self.settings.read_text())
        settings["only"] = "MG12"
        for name in ("001", "002"):
            attempt = attempts / name
            (attempt / "raw").mkdir(parents=True)
            (attempt / "settings.json").write_text(json.dumps(settings))
            (attempt / "raw/results.tsv").write_text("MG12\ttp3\tpass\tok\nMG12\tpin\tpass\tok\n")
            for file in mg.required_evidence("MG12", settings):
                (attempt / "raw" / file).write_text("{}\n" if file.endswith("jsonl") else "evidence\n")
            (attempt / "raw/versions.txt").write_text((self.raw / "versions.txt").read_text())
            (attempt / "host-completion.json").write_text('{"exit_code":0}')
            (attempt / "completion.json").write_text('{"state":"complete","host_exit":0,"final_collection":true}')
        settings["only"] = "MG14"
        self.settings.write_text(json.dumps(settings))
        (self.raw / "results.tsv").write_text("")
        args = argparse.Namespace(results=str(self.raw / "results.tsv"), out=str(self.root / "verdict.md"),
                                  settings=str(self.settings), only="MG14", attempts=str(attempts))
        latest = attempts / "002"
        for missing in ("completion.json", "raw/versions.txt"):
            with self.subTest(missing=missing):
                path = latest / missing
                saved = path.read_text()
                path.unlink()
                with contextlib.redirect_stdout(io.StringIO()):
                    mg.cmd_verdict(args)
                row = json.loads((self.root / "verdict.json").read_text())["rows"]["MG12"]
                self.assertEqual(row["attempt"], "002")
                self.assertEqual(row["result"], "fail")
                path.write_text(saved)


class RunnerTests(unittest.TestCase):
    """Run the real foreground runner against a local, bounded transport fixture."""
    def setUp(self):
        scratch = Path.home() / ".cache/capyctl-agent-tmp/mg-tests"
        scratch.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.tmp = tempfile.TemporaryDirectory(dir=scratch)
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.fixture = self.root / "fixture"
        self.fixture.mkdir()
        for row in mg.EXPECTED:
            for name in mg.required_evidence(row, {}):
                (self.fixture / name).write_text("{}\n" if name.endswith((".json", ".jsonl")) else "evidence\n")
        (self.fixture / "versions.txt").write_text("approved engine fingerprints\n")
        for role in ("s1", "s2", "h1"):
            logs = self.fixture / "engine-logs" / role
            logs.mkdir(parents=True)
            (logs / "engine.log").write_text("engine startup evidence\n")
        (self.fixture / "MG3-fit.txt").write_text("CUDA0 model buffer size = 1 MiB\nCUDA1 model buffer size = 1 MiB\n")
        (self.fixture / "MG3-charge.json").write_text(json.dumps({"gpu0": [{"bytes": 2**20}], "gpu1": [{"bytes": 2**20}]}))
        (self.fixture / "results.tsv").write_text("".join(f"{row}\t{part}\tpass\tevidence\n"
                                                        for row, parts in mg.EXPECTED.items() for part in parts))
        transport = self.bin / "transport"
        transport.write_text(textwrap.dedent('''\
            #!/usr/bin/env python3
            import json, os, pathlib, re, shutil, subprocess, sys
            root = pathlib.Path(os.environ["MG_FIXTURE_ROOT"])
            mode = os.environ["MG_FIXTURE_MODE"]
            host = root / "host"
            host.mkdir(exist_ok=True)
            def counter(name):
                path = root / name
                value = int(path.read_text()) + 1 if path.exists() else 1
                path.write_text(str(value))
                return value
            if pathlib.Path(sys.argv[0]).name == "ssh":
                command = sys.argv[-1]
                match = re.search(r"ATTEMPT_ID=([A-Za-z0-9_-]+)", command)
                attempt = match.group(1) if match else None
                if "mkdir -m" in command:
                    attempt = re.search(r"attempts/([^/]+)/raw", command).group(1)
                    (host / "mg/live").mkdir(parents=True, exist_ok=True)
                    (host / f"mg/attempts/{attempt}/raw").mkdir(parents=True)
                elif command.endswith(" preflight"):
                    print("PREFLIGHT_OK")
                elif command.endswith(" collect"):
                    calls = counter("collect-count")
                    if mode == "disconnect" and calls >= 3:
                        sys.exit(255)
                    subprocess.run([sys.executable, str(host / "mg/live/mg.py"), "snapshot",
                                    "--root", str(host / "mg"), "--attempt", attempt], check=True)
                elif "nohup timeout" in command:
                    target = host / f"mg/attempts/{attempt}"
                    shutil.copytree(root / "fixture", target / "raw", dirs_exist_ok=True)
                    if mode == "fail":
                        result = target / "raw/results.tsv"
                        result.write_text(result.read_text().replace("MG5\\tdeep\\tpass", "MG5\\tdeep\\tfail"))
                    if mode == "missing":
                        (target / "raw/MG3-fit.txt").unlink()
                    if mode == "missing-enrolled":
                        (target / "raw/MG11-owned.jsonl").unlink()
                    if mode != "disconnect":
                        (target / "host-completion.json").write_text(json.dumps(dict(exit_code=0)))
                elif command.endswith(" finalize"):
                    if mode == "purge-fail":
                        sys.exit(1)
                    export = host / "mg/evidence/old/raw"
                    export.mkdir(parents=True, exist_ok=True)
                    (export / "late.log").write_text("final shutdown evidence")
                elif command.endswith(" purge"):
                    (root / "purged").write_text("yes")
                else:
                    raise SystemExit("unexpected fixture SSH command")
            else:
                args = []
                skip = False
                for arg in sys.argv[1:]:
                    if skip:
                        skip = False
                    elif arg == "-e":
                        skip = True
                    elif not arg.startswith("-"):
                        args.append(arg)
                sources, destination = args[:-1], args[-1]
                downloading = ":mg/evidence/" in sources[0]
                if downloading and mode == "copy-fail" and counter("download-count") >= 3:
                    sys.exit(23)
                def local(path):
                    return host / path.split(":", 1)[1] if ":mg/" in path else pathlib.Path(path)
                destination = local(destination)
                destination.mkdir(parents=True, exist_ok=True)
                for source in sources:
                    path = local(source)
                    if path.is_dir():
                        shutil.copytree(path, destination, dirs_exist_ok=True)
                    else:
                        shutil.copy2(path, destination / path.name)
            '''))
        transport.chmod(0o700)
        for name in ("ssh", "rsync"):
            (self.bin / name).symlink_to(transport.name)

    def run_fixture(self, mode, only="all"):
        out = self.root / "output"
        env = dict(os.environ, PATH=str(self.bin) + os.pathsep + os.environ["PATH"],
                   MG_FIXTURE_ROOT=str(self.root), MG_FIXTURE_MODE=mode)
        result = subprocess.run(["bash", str(HERE / "run.sh"), "--ssh", "fixture@203.0.113.9",
                                 "--skip-setup", "--out", str(out), "--max-seconds", "8",
                                 "--disconnect-seconds", "1", "--poll-seconds", "1", "--only", only] +
                                (["--purge-only"] if mode.startswith("purge-") else []),
                                env=env, capture_output=True, text=True, timeout=30)
        completion = json.loads((out / "completion.json").read_text())
        self.assertEqual(completion["state"], "complete", result.stdout + result.stderr)
        self.assertEqual(completion["exit_code"], result.returncode)
        return result, completion, out

    def test_success_exits_after_final_copy_and_purge(self):
        result, completion, out = self.run_fixture("success")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(completion["final_collection"])
        self.assertTrue(completion["purged"])
        self.assertTrue(json.loads((out / "verdict.json").read_text())["qualified"])

    def test_enrolled_only_collects_full_serving_evidence(self):
        result, _, out = self.run_fixture("success", only="MG11")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        report = json.loads((out / "verdict.json").read_text())
        self.assertTrue(report["selected_pass"])
        self.assertFalse(report["qualified"])
        self.assertEqual(report["rows"]["MG11"]["result"], "pass")

    def test_failing_row_exits_nonzero_despite_zero_host_exit(self):
        result, completion, _ = self.run_fixture("fail")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(completion["host_exit"], 0)
        self.assertTrue(completion["purged"])

    def test_missing_evidence_exits_nonzero(self):
        result, _, out = self.run_fixture("missing")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(json.loads((out / "verdict.json").read_text())["qualified"])

    def test_missing_enrolled_ownership_cannot_qualify(self):
        result, _, out = self.run_fixture("missing-enrolled")
        self.assertNotEqual(result.returncode, 0)
        report = json.loads((out / "verdict.json").read_text())
        self.assertFalse(report["qualified"])
        self.assertIn("MG11-owned.jsonl", report["rows"]["MG11"]["missing_evidence"])

    def test_interrupted_connection_preserves_last_copy_and_does_not_purge(self):
        result, completion, out = self.run_fixture("disconnect")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(completion["purged"])
        self.assertFalse(json.loads((out / "verdict.json").read_text())["qualified"])
        self.assertTrue(list((out / "attempts").glob("*/raw/results.tsv")))
        self.assertFalse((self.root / "purged").exists())

    def test_failed_final_copy_prevents_purge(self):
        result, completion, out = self.run_fixture("copy-fail")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(completion["final_collection"])
        self.assertFalse(json.loads((out / "verdict.json").read_text())["qualified"])
        self.assertFalse((self.root / "purged").exists())
        self.assertTrue(list((out / "attempts").glob("*/raw/results.tsv")))

    def test_purge_only_collects_finalized_evidence(self):
        result, completion, out = self.run_fixture("purge-success")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(completion["purged"])
        self.assertEqual((out / "attempts/old/raw/late.log").read_text(), "final shutdown evidence")

    def test_purge_only_refuses_failed_finalization(self):
        result, _, _ = self.run_fixture("purge-fail")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.root / "purged").exists())


if __name__ == "__main__":
    unittest.main()
