"""Fetch the schema corpus.

Run on purpose, once. Not run by the tests: a suite that quietly downloads
several megabytes is a suite that behaves differently on a machine with no
network, and the corpus tests skip cleanly when the files are absent.

Two kinds of source.

A **file** is one URL holding a whole schema — a Rails or Ecto `structure.sql`,
or a squashed migration set. Simple, and the best kind when a project has one.

A **directory** is a migrations folder, which is where most projects that are
not Rails keep their schema. The files are listed through the GitHub contents
API and concatenated in name order, which is a *replay* rather than a snapshot:
a table created early and altered later only arrives in its final state if
every migration in between applies. The corpus harness tolerates statement
failures and counts the constraints they cost, so a replay that goes wrong is
visible rather than silent.
"""
import json
import os
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
AGENT = "pgsow-corpus/0.1 (https://github.com/Blahaj-gif/pgsow)"
API = "https://api.github.com/repos/{repo}/contents/{path}"


# Every schema is pinned to a commit. It used to be fetched from whatever a
# branch pointed at, which meant CI and a laptop measured different corpora and
# the difference showed up as a schema mysteriously losing nineteen more
# constraints on one machine than the other. A benchmark that changes under you
# is not a benchmark, and the numbers in the README are only worth printing if
# somebody else can get them. Moving a pin is a commit anybody can review.


def get(url, accept=None):
    headers = {"User-Agent": AGENT}
    if accept:
        headers["Accept"] = accept
    # A token lifts the unauthenticated rate limit of 60 requests an hour,
    # which a nested directory can reach on its own. Optional: without one this
    # still works, just less often.
    token = os.environ.get("GITHUB_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(request, timeout=180) as response:
        return response.read()


def listing(repo, path, ref=None):
    """One directory, as (name, type, download_url) sorted by name."""
    url = API.format(repo=repo, path=path)
    if ref:
        url += f"?ref={ref}"
    entries = json.loads(get(url, "application/vnd.github+json"))
    return sorted(
        (e["name"], e["type"], e.get("download_url")) for e in entries
    )


def fetch_directory(schema):
    """Concatenate every matching file in a migrations directory."""
    repo, path = schema["repo"], schema["path"]
    ref = schema.get("ref")
    suffix = schema.get("suffix", ".sql")
    # Projects that keep one file per dialect often name the shared one plainly
    # and the others `.cockroach.up.sql`, `.mysql.up.sql`. The suffix alone
    # cannot tell them apart, so the ones to leave out are named.
    excluded = schema.get("exclude", [])
    keep = lambda name: name.endswith(suffix) and not any(x in name for x in excluded)
    parts = []
    for name, kind, url in listing(repo, path, ref):
        if kind == "dir":
            # One level down, for projects that give each migration its own
            # folder. Not recursive beyond that: nothing needs it, and a
            # runaway walk of a large repository is a poor way to find out.
            if not schema.get("nested"):
                continue
            for inner_name, inner_kind, inner_url in listing(repo, f"{path}/{name}", ref):
                if inner_kind == "file" and keep(inner_name):
                    parts.append((f"{name}/{inner_name}", inner_url))
        elif keep(name):
            parts.append((name, url))

    out = []
    for name, url in parts:
        out.append(f"-- {name}\n".encode())
        out.append(get(url))
        out.append(b"\n")
    print(f"       {len(parts)} files from {repo}/{path}")
    return b"".join(out)


for schema in json.load(open(os.path.join(HERE, "sources.json"), encoding="utf-8"))["schemas"]:
    target = os.path.join(HERE, schema["file"])
    if os.path.exists(target):
        print(f"  have {schema['file']}")
        continue
    data = fetch_directory(schema) if schema.get("kind") == "directory" else get(schema["url"])
    with open(target, "wb") as handle:
        handle.write(data)
    print(f"  got  {schema['file']}  {len(data):,} bytes  ({schema['project']}, {schema['licence']})")
