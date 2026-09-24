#!/usr/bin/env python3
"""Read-only view of the server's accounting for E0 evidence (plan unit W2).

The database is opened with mode=ro and query_only; only SELECT statements over
an allowlist of accounting tables are issued. Secret-bearing tables (engine
secrets, certificates, invitations) and binding payloads are never read.

  ledger.py snapshot    --db DB                       accounting snapshot as JSON
  ledger.py owned       --db DB --deployment NAME [--role ROLE]
                        owned process identities (pid, boot id, start ticks, host)
  ledger.py fingerprint --db DB --deployment NAME     content_fingerprint of the current revision
  ledger.py accounting  --db DB --deployment NAME     one deployment's charges, claims, leases,
                        retained bindings and checkpoint digest rows (M16 cleanup evidence)
  ledger.py evidence    --db DB --deployment NAME     the deployment's operations, lifecycle steps with
                        their recorded evidence (park/restore residency, cleanup) and journal
                        entries, with every credential-shaped field removed
"""

import argparse
import json
import sqlite3
import sys


def connect(path):
    db = sqlite3.connect(f"file:{path}?mode=ro", uri=True, timeout=5)
    db.execute("PRAGMA query_only=1")
    db.row_factory = sqlite3.Row
    return db


def rows(db, sql, params=()):
    assert sql.lstrip().upper().startswith("SELECT")
    return [dict(row) for row in db.execute(sql, params)]


def deployment_id(db, name):
    found = rows(db, "SELECT id FROM deployments WHERE name=? OR id=?", (name, name))
    if not found:
        sys.exit(f"no deployment {name}")
    return found[0]["id"]


def snapshot(db):
    out = {
        "deployments": rows(db, "SELECT id,name,desired_state,current_generation,admission_enabled,suspended,updated_at FROM deployments ORDER BY name"),
        "reservations": rows(db, "SELECT owner_id,domain_id,bytes,phase FROM reservations ORDER BY owner_id"),
        "resource_owners": rows(db, "SELECT owner_id,footprint_json FROM resource_owners ORDER BY owner_id"),
        "resource_grants": rows(db, "SELECT count(*) AS grants, max(committed_epoch) AS epoch FROM resource_grants"),
        "endpoint_leases": rows(db, "SELECT host,port,binding_id FROM endpoint_leases ORDER BY host,port"),
        "lifecycle_claims": rows(db, "SELECT deployment_id,operation_id,revision,generation FROM lifecycle_claims"),
        "request_leases": rows(db, "SELECT id,deployment_id,revision,generation,disposition FROM request_leases"),
        "runtime_bindings": rows(db, "SELECT b.id,b.deployment_id,b.revision,b.incarnation,b.ownership,b.state,b.identities_json,"
                                     "i.host_id FROM runtime_bindings b LEFT JOIN remote_binding_ingress i ON i.binding_id=b.id "
                                     "WHERE b.state!='released' ORDER BY b.deployment_id"),
        "checkpoint_digests": rows(db, "SELECT deployment_id,revision,state,host_id,digest,weights_bytes,provisional,"
                                       "diagnostic,updated_at_ms FROM checkpoint_digests ORDER BY deployment_id,revision"),
        "operations_recent": rows(db, "SELECT id,deployment_id,kind,state,error_code,updated_at FROM operations ORDER BY rowid DESC LIMIT 16"),
    }
    for binding in out["runtime_bindings"]:
        binding["identities"] = json.loads(binding.pop("identities_json") or "[]")
    for owner in out["resource_owners"]:
        owner["footprint"] = json.loads(owner.pop("footprint_json"))
    return out


def owned(db, name, role):
    ident = deployment_id(db, name)
    bindings = rows(db, "SELECT b.id,b.state,b.identities_json,i.host_id FROM runtime_bindings b "
                        "LEFT JOIN remote_binding_ingress i ON i.binding_id=b.id "
                        "WHERE b.deployment_id=? AND b.state IN ('live','uncertain')", (ident,))
    result = []
    for binding in bindings:
        for identity in json.loads(binding["identities_json"] or "[]"):
            if role and identity.get("role") != role:
                continue
            result.append({"deployment_id": ident, "binding_id": binding["id"], "binding_state": binding["state"],
                           "host_id": binding["host_id"], **identity})
    return result


def accounting(db, name):
    ident = deployment_id(db, name)
    bindings = rows(db, "SELECT id,state FROM runtime_bindings WHERE deployment_id=? AND state!='released'", (ident,))
    return {
        "deployment_id": ident,
        "deployment": rows(db, "SELECT desired_state,observed_state,current_generation,revision,admission_enabled,"
                               "dispatch_enabled,suspended FROM deployments WHERE id=?", (ident,)),
        # The ledger's charge (per instance owner; instance 0 is the deployment id).
        "resource_owners": [{"owner_id": o["owner_id"], "footprint": json.loads(o["footprint_json"])} for o in
                            rows(db, "SELECT owner_id,footprint_json FROM resource_owners WHERE owner_id=? OR owner_id LIKE ?",
                                 (ident, f"deployment:{ident}/instance:%"))],
        "reservations": rows(db, "SELECT owner_id,domain_id,bytes,phase FROM reservations WHERE owner_id=?", (ident,)),
        "lifecycle_claims": rows(db, "SELECT operation_id,revision,generation FROM lifecycle_claims WHERE deployment_id=?", (ident,)),
        "request_leases_deployment": rows(db, "SELECT id,generation,disposition FROM request_leases WHERE deployment_id=?", (ident,)),
        "request_leases_total": rows(db, "SELECT count(*) AS n FROM request_leases")[0]["n"],
        "retained_bindings": bindings,
        "endpoint_leases": rows(db, "SELECT l.host_id,l.host,l.port FROM endpoint_leases l JOIN runtime_bindings b "
                                    "ON b.id=l.binding_id WHERE b.deployment_id=?", (ident,)),
        "checkpoint_digests": rows(db, "SELECT revision,state,host_id,digest,weights_bytes,provisional,diagnostic,updated_at_ms "
                                       "FROM checkpoint_digests WHERE deployment_id=? ORDER BY revision", (ident,)),
    }


SECRETISH = ("key", "token", "secret", "password", "credential", "bearer", "authorization")


def redact(node):
    if isinstance(node, dict):
        return {k: ("<redacted>" if any(w in k.lower() for w in SECRETISH) else redact(v)) for k, v in node.items()}
    if isinstance(node, list):
        return [redact(v) for v in node]
    return node


def parse_json(text):
    try:
        return redact(json.loads(text))
    except (TypeError, ValueError):
        return "<unparsed>"


def evidence(db, name):
    ident = deployment_id(db, name)
    ops = rows(db, "SELECT id,kind,state,error_code,updated_at FROM operations WHERE deployment_id=? ORDER BY rowid", (ident,))
    steps = rows(db, "SELECT s.id,s.operation_id,s.ordinal,s.binding_id,s.state,s.step_json,e.evidence_json,e.committed_epoch "
                     "FROM lifecycle_steps s LEFT JOIN lifecycle_evidence e ON e.step_id=s.id "
                     "WHERE s.deployment_id=? ORDER BY s.rowid", (ident,))
    for step in steps:
        step["step"] = parse_json(step.pop("step_json"))
        raw = step.pop("evidence_json")
        step["evidence"] = parse_json(raw) if raw is not None else None
    op_ids = [o["id"] for o in ops]
    journal = []
    if op_ids:
        marks = ",".join("?" for _ in op_ids)
        journal = rows(db, f"SELECT id,host_id,operation_id,state,evidence,recorded_at FROM journal_entries "
                           f"WHERE operation_id IN ({marks}) OR evidence LIKE ? ORDER BY recorded_at", (*op_ids, f"%{ident}%"))
        for entry in journal:
            entry["evidence"] = parse_json(entry["evidence"])
    return {"deployment_id": ident, "operations": ops, "steps": steps, "journal": journal}


def find_key(node, key):
    if isinstance(node, dict):
        if key in node:
            return node[key]
        node = list(node.values())
    if isinstance(node, list):
        for value in node:
            found = find_key(value, key)
            if found is not None:
                return found
    return None


def fingerprint(db, name):
    ident = deployment_id(db, name)
    found = rows(db, "SELECT revision,effective_json FROM effective_revisions WHERE deployment_id=? "
                     "ORDER BY revision DESC LIMIT 1", (ident,))
    if not found:
        sys.exit(f"no effective revision for {name}")
    effective = json.loads(found[0]["effective_json"])
    return {"deployment_id": ident, "revision": found[0]["revision"],
            "content_fingerprint": find_key(effective, "content_fingerprint")}


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("cmd", choices=["snapshot", "owned", "fingerprint", "accounting", "evidence"])
    parser.add_argument("--db", required=True)
    parser.add_argument("--deployment")
    parser.add_argument("--role")
    args = parser.parse_args()
    db = connect(args.db)
    if args.cmd == "snapshot":
        result = snapshot(db)
    elif not args.deployment:
        parser.error("--deployment is required")
    elif args.cmd == "owned":
        result = owned(db, args.deployment, args.role)
    elif args.cmd == "accounting":
        result = accounting(db, args.deployment)
    elif args.cmd == "evidence":
        result = evidence(db, args.deployment)
    else:
        result = fingerprint(db, args.deployment)
    json.dump(result, sys.stdout, indent=1, sort_keys=True)
    print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
