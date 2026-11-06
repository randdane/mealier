#!/usr/bin/env python3 -I
"""Delete ONLY objects whose name starts with "mealier-fixture". NOT run by the seeding script.
Usage: python3 -I cleanup-mealie-fixture.py [--yes]   (dry run without --yes)"""
import json, os, sys, urllib.error, urllib.request
from pathlib import Path

BASE, P = "https://mealie.example.com", "mealier-fixture"
XDG = Path(os.environ.get("XDG_DATA_HOME") or Path.home() / ".local/share")
TOKEN = (XDG / "_r_/mealie/token").read_text().strip()
YES = "--yes" in sys.argv


def call(m, path):
    req = urllib.request.Request(BASE + path, method=m, headers={"Authorization": f"Bearer {TOKEN}"})
    with urllib.request.urlopen(req, timeout=90) as r:
        b = r.read()
        return json.loads(b) if b else None


def items(path):
    r = call("GET", f"{path}?perPage=-1&page=1")
    return r["items"] if isinstance(r, dict) else r


# (list path, name field, delete path prefix); order matters: dependents first
for lp, field, dp in (
    ("/api/households/shopping/lists", "name", "/api/households/shopping/lists"),
    ("/api/households/mealplans", "title", "/api/households/mealplans"),
    ("/api/households/cookbooks", "name", "/api/households/cookbooks"),
    ("/api/recipes", "name", None),  # deleted by slug; cascades timeline events and comments
    ("/api/organizers/tags", "name", "/api/organizers/tags"),
    ("/api/organizers/categories", "name", "/api/organizers/categories"),
    ("/api/organizers/tools", "name", "/api/organizers/tools"),
    ("/api/foods", "name", "/api/foods"),
    ("/api/units", "name", "/api/units"),
    ("/api/groups/labels", "name", "/api/groups/labels"),
    ("/api/admin/users", "username", "/api/admin/users"),
):
    for it in items(lp):
        if (it.get(field) or "").startswith(P):
            path = f"/api/recipes/{it['slug']}" if dp is None else f"{dp}/{it['id']}"
            print(("DELETE " if YES else "would delete ") + path, it[field])
            if YES:
                try: call("DELETE", path)
                except urllib.error.HTTPError as e: print("  failed", e.code)
