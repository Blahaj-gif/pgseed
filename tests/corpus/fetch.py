"""Fetch the schema corpus.

Run on purpose, once. Not run by the tests: a suite that quietly downloads
several megabytes is a suite that behaves differently on a machine with no
network, and the corpus tests skip cleanly when the files are absent.

Two kinds of source.

A **file** is one URL holding a whole schema — a Rails or Ecto `structure.sql`,
or a squashed migration set. Simple, and the best kind when a project has one.

A **directory** is a migrations folder, which is where most projects that are
not Rails keep their schema. The files are listed through the GitHub git trees
API and concatenated in name order, which is a *replay* rather than a snapshot:
a table created early and altered later only arrives in its final state if
every migration in between applies. The corpus harness tolerates statement
failures and counts the constraints they cost, so a replay that goes wrong is
visible rather than silent.
"""
import json
import os
import urllib.parse
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
AGENT = "pgsow-corpus/0.1 (https://github.com/Blahaj-gif/pgsow)"
TREE = "https://api.github.com/repos/{repo}/git/trees/{ref}:{path}?recursive=1"
RAW = "https://raw.githubusercontent.com/{repo}/{ref}/{path}/{name}"


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
    # which the old per-directory walk could reach on a single schema.
    # Optional: without one this still works, just less often.
    token = os.environ.get("GITHUB_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(request, timeout=180) as response:
        return response.read()


def tree(repo, path, ref):
    """Every file under `path`, as paths relative to it, in **one** API call.

    This used to walk the contents API one directory at a time. Prisma gives
    each migration its own folder, so Langfuse alone cost 433 listing calls —
    impossible unauthenticated and slow with a token. The trees API takes a
    `ref:path` tree-ish, which scopes the recursion to the migrations folder
    rather than the repository, so it is one call and cannot truncate on a
    large repository the way a whole-tree fetch can.
    """
    listing = json.loads(
        get(TREE.format(repo=repo, ref=ref, path=path), "application/vnd.github+json")
    )
    # A truncated listing is a partial corpus, and a partial corpus that says
    # nothing is a benchmark that quietly measures less than it claims.
    if listing.get("truncated"):
        raise SystemExit(f"{repo}/{path}: the tree listing was truncated")
    return [entry["path"] for entry in listing["tree"] if entry["type"] == "blob"]


def ordering(relative):
    """Sort a relative path the way one directory listing at a time did.

    The old walk sorted the top level, then sorted inside each folder it
    descended into, so a folder sorted against its siblings by its own name and
    not by the files inside it. Sorting the joined path instead would order
    `b.sql` before `b/c.sql`, because `.` sorts below `/`, and the four
    directory schemas already in the corpus would change. Splitting the first
    component off reproduces the old order exactly.
    """
    head, _, rest = relative.partition("/")
    return (head, rest)


def fetch_directory(schema):
    """Concatenate every matching file in a migrations directory."""
    repo, path = schema["repo"], schema["path"]
    ref = schema.get("ref")
    if not ref:
        raise SystemExit(f"{repo}/{path}: a directory source must pin a ref")
    suffix = schema.get("suffix", ".sql")
    # Projects that keep one file per dialect often name the shared one plainly
    # and the others `.cockroach.up.sql`, `.mysql.up.sql`. The suffix alone
    # cannot tell them apart, so the ones to leave out are named.
    excluded = schema.get("exclude", [])
    # And where the filter is naturally positive — Camunda ships six dialects
    # in one folder and only `postgres` is wanted, name what to keep instead.
    # An exclude list of the other five would silently admit a seventh.
    required = schema.get("contains")

    def keep(name):
        if not name.endswith(suffix) or any(x in name for x in excluded):
            return False
        return required is None or required in name

    parts = []
    for relative in sorted(tree(repo, path, ref), key=ordering):
        # One level down, for projects that give each migration its own folder.
        # Not deeper: nothing needs it, and a runaway walk of a large
        # repository is a poor way to find out.
        depth = relative.count("/")
        if depth > 1 or (depth == 1 and not schema.get("nested")):
            continue
        if keep(relative.rsplit("/", 1)[-1]):
            parts.append(relative)

    out = []
    for name in parts:
        out.append(f"-- {name}\n".encode())
        # Escaped, because a filename is whatever somebody typed: Langfuse has
        # a migration folder called `20240126184148_new_models copy`, and a raw
        # space in a request line is not a URL. The contents API handed over an
        # already-escaped `download_url` and so hid this.
        body = get(RAW.format(repo=repo, ref=ref, path=path,
                              name=urllib.parse.quote(name)))
        out.append(body)
        # A file boundary is a statement boundary: every migration runner
        # executes each file on its own, so the last statement in a file needs
        # no terminator and Prisma often leaves it off. Concatenated, that
        # statement swallows the first one of the next file. Langfuse merged an
        # `INSERT INTO models` with the `ALTER TABLE ... ADD COLUMN project_id`
        # that followed it, the merged statement read as an INSERT and was
        # filtered out with the seed data, and seven constraints downstream of
        # that column were lost. Supplied only when it is missing, so a schema
        # whose files all terminate properly is untouched.
        out.append(b"\n" if body.rstrip().endswith(b";") else b"\n;\n")
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
