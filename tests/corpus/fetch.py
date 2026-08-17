"""Fetch the schema corpus.

Run on purpose, once. Not run by the tests: a suite that quietly downloads
several megabytes is a suite that behaves differently on a machine with no
network, and the corpus tests skip cleanly when the files are absent.
"""
import json
import os
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
AGENT = "pgsow-corpus/0.1 (https://github.com/Blahaj-gif/pgsow)"

for schema in json.load(open(os.path.join(HERE, "sources.json"), encoding="utf-8"))["schemas"]:
    target = os.path.join(HERE, schema["file"])
    if os.path.exists(target):
        print(f"  have {schema['file']}")
        continue
    request = urllib.request.Request(schema["url"], headers={"User-Agent": AGENT})
    with urllib.request.urlopen(request, timeout=180) as response:
        data = response.read()
    with open(target, "wb") as handle:
        handle.write(data)
    print(f"  got  {schema['file']}  {len(data):,} bytes  ({schema['project']}, {schema['licence']})")
