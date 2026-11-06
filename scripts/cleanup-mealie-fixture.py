#!/usr/bin/env python3 -I
"""Delete ONLY objects whose name starts with "mealier-fixture" (plus the meal-plan rules and share links
that hang off fixture tags and recipes). NOT run by the seeding script.
Usage: python3 -I cleanup-mealie-fixture.py [--yes]   (dry run without --yes)"""
import json, os, sys, urllib.error, urllib.request
from pathlib import Path

BASE, P = os.environ["MEALIE_URL"].rstrip("/"), "mealier-fixture"
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


def delete(path, label):
    print(("DELETE " if YES else "would delete ") + path, label)
    if YES:
        try: call("DELETE", path)
        except urllib.error.HTTPError as e: print("  failed", e.code)


fixture_ids = {t["id"] for lp in ("/api/organizers/tags", "/api/organizers/categories") for t in items(lp) if t["name"].startswith(P)}
fixture_recipes = [r for r in items("/api/recipes") if r["name"].startswith(P)]

# meal-plan rules have no name: ours are the ones whose filter names a fixture tag or category
for r in items("/api/households/mealplans/rules"):
    if any(i in r["queryFilterString"] for i in fixture_ids):
        delete(f"/api/households/mealplans/rules/{r['id']}", f"rule {r['day']} {r['entryType']}")
# share links hang off recipes; delete them before the recipes
for r in fixture_recipes:
    for t in call("GET", f"/api/shared/recipes?recipe_id={r['id']}") or []:
        delete(f"/api/shared/recipes/{t['id']}", f"share link of {r['name']}")

# (list path, name field, delete path prefix); order matters: dependents first
for lp, field, dp in (
    ("/api/households/events/notifications", "name", "/api/households/events/notifications"),
    ("/api/households/webhooks", "name", "/api/households/webhooks"),
    ("/api/households/recipe-actions", "title", "/api/households/recipe-actions"),
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
    ("/api/admin/users", "username", "/api/admin/users"),  # takes its meal plan entries and lists with it
    ("/api/admin/households", "name", "/api/admin/households"),  # after the users: a household with users cannot go
    ("/api/admin/groups", "name", "/api/admin/groups"),
):
    for it in items(lp):
        if (it.get(field) or "").startswith(P):
            delete(f"/api/recipes/{it['slug']}" if dp is None else f"{dp}/{it['id']}", it[field])
