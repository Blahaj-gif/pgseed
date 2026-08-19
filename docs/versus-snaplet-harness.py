"""Snaplet seed and pgseed, against the same schemas in the same database.

Fair by construction: one database per schema, loaded once from the same file,
and each tool asked for the same number of rows into a database the others have
not touched. Nothing here reads a tool's own report for its score — the count
comes from the database afterwards, which is the only witness all three runs
can share.

Three runs per schema, because two of them answer different questions:

  snaplet        writes rows and finds out what the database says
  pgseed         writes only what it can prove, and names the rest
  pgseed --probe writes what it can prove, then offers the rest to the database
                 behind a savepoint — the same bet snaplet makes, so this is
                 the like-for-like column
"""
import json
import os
import re
import subprocess
import sys
import time

PG = r"C:\Users\LEGION\.theseus\postgresql\18.6.0\bin"
HERE = r"C:\Users\LEGION\AppData\Local\Temp\claude\c--mcp-servers\7b06f469-c04f-48a9-bdb0-49a7e2b8fcf6\scratchpad"
SNAP = os.path.join(HERE, "snaplet")
CORPUS = r"C:\mcp-servers\pgsow\tests\corpus"
BIN = r"C:\mcp-servers\pgsow\target\release\pgseed.exe"
BASE = "postgres://postgres:snaplet@127.0.0.1:55432"
ROWS = "5"

# One short query however many tables there are. Building a UNION of per-table
# counts is the obvious way and it died on GitLab: 1,057 SELECTs is longer than
# Windows will accept on a command line. `query_to_xml` runs the count inside
# the server instead, so the text sent over is a fixed size.
COUNTS = """
SELECT count(*) FILTER (WHERE n > 0), count(*)
FROM (
  SELECT (xpath('/row/c/text()',
           query_to_xml(format('SELECT count(*) AS c FROM %I.%I', ns.nspname, c.relname),
                        false, true, '')))[1]::text::bigint AS n
  FROM pg_class c
  JOIN pg_namespace ns ON ns.oid = c.relnamespace
  WHERE c.relkind = 'r'
    AND ns.nspname NOT IN ('pg_catalog', 'information_schema')
) s
"""

TRUNCATE = """
DO $$ DECLARE r record; BEGIN
  FOR r IN SELECT c.oid::regclass AS t
           FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
           WHERE c.relkind = 'r' AND n.nspname NOT IN ('pg_catalog','information_schema')
  LOOP BEGIN EXECUTE 'TRUNCATE ' || r.t || ' CASCADE'; EXCEPTION WHEN OTHERS THEN NULL; END;
  END LOOP;
END $$;
"""


def psql(db, *args, timeout=1800):
    return subprocess.run(
        [os.path.join(PG, "psql.exe"), f"{BASE}/{db}", *args],
        capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=timeout)


def counts(db):
    out = psql(db, "-tAF,", "-c", COUNTS)
    try:
        filled, total = out.stdout.strip().splitlines()[0].split(",")
        return int(filled), int(total)
    except Exception:
        return -1, -1


def truncate_all(db):
    psql(db, "-tAc", TRUNCATE)


def run_snaplet(db):
    env = dict(os.environ, DATABASE_URL=f"{BASE}/{db}")
    sync = subprocess.run(["npx", "@snaplet/seed", "sync"], cwd=SNAP, env=env, shell=True,
                          capture_output=True, text=True, encoding="utf-8",
                          errors="replace", timeout=2400)
    blob = (sync.stdout or "") + (sync.stderr or "")
    if "Database structure analyzed" not in blob:
        return {"error": "sync failed: " + blob.strip()[-160:]}
    seed = subprocess.run(["npx", "tsx", "seed.ts"], cwd=SNAP, env=dict(env, ROWS=ROWS),
                          shell=True, capture_output=True, text=True, encoding="utf-8",
                          errors="replace", timeout=3600)
    m = re.search(r"^RESULT (.*)$", seed.stdout or "", re.M)
    if not m:
        return {"error": "seed failed: " + ((seed.stderr or seed.stdout or "").strip()[-160:])}
    return {"models": json.loads(m.group(1))}


def run_pgseed(db, probe):
    argv = [BIN, "--dsn", f"{BASE}/{db}", "--apply", "--rows", ROWS, "--allow-nonempty"]
    if probe:
        argv.append("--probe")
    p = subprocess.run(argv, capture_output=True, text=True, encoding="utf-8",
                       errors="replace", timeout=3600)
    return p.returncode


def main(names):
    results = {}
    out_path = os.path.join(HERE, "headtohead.json")
    for name in names:
        path = os.path.join(CORPUS, f"{name}.sql")
        if not os.path.exists(path):
            continue
        started = time.time()
        psql("postgres", "-c", f'DROP DATABASE IF EXISTS "{name}"',
             "-c", f'CREATE DATABASE "{name}"')
        # Schemas first: a dump may put its tables in one it assumes exists,
        # and Hasura's does. Without this the whole file fails and both tools
        # score zero for a reason that is about the harness.
        head = open(path, encoding="utf-8", errors="replace").read(4_000_000)
        for ns in sorted({m for m in re.findall(
                r"CREATE TABLE (?:IF NOT EXISTS )?\"?([a-zA-Z_][a-zA-Z_0-9]*)\"?\.", head)}):
            psql(name, "-c", f'CREATE SCHEMA IF NOT EXISTS "{ns}"')
        psql(name, "-f", path)

        _, total = counts(name)
        row = {"tables": total}

        truncate_all(name)
        try:
            snap = run_snaplet(name)
        except subprocess.TimeoutExpired:
            snap = {"error": "timed out"}
        if "models" in snap:
            failed = {k: v for k, v in snap["models"].items() if v != "ok"}
            row["snaplet_filled"] = counts(name)[0]
            row["snaplet_failed"] = len(failed)
            row["snaplet_errors"] = sorted({v[:100] for v in failed.values()})[:6]
        else:
            row["snaplet_error"] = snap["error"]

        for label, probe in (("pgseed", False), ("pgseed_probe", True)):
            truncate_all(name)
            try:
                code = run_pgseed(name, probe)
            except subprocess.TimeoutExpired:
                code = -9
            row[label + "_filled"] = counts(name)[0]
            row[label + "_code"] = code

        row["seconds"] = round(time.time() - started, 1)
        results[name] = row
        print(name, json.dumps(row)[:300], flush=True)
        # Written every time, so a crash on the last schema does not throw away
        # the twenty-three before it. That happened once.
        with open(out_path, "w", encoding="utf-8") as f:
            json.dump(results, f, indent=2)


if __name__ == "__main__":
    main(sys.argv[1:])
