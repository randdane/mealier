#!/usr/bin/env python3 -I
"""Scrub a Mealie v3.28 backup zip into the committed test fixture.

Usage: python3 -I scripts/make-mealie-fixture.py INPUT.zip [OUTPUT.zip]
OUTPUT defaults to tests/fixtures/mealie_v3.28.0.zip (relative to the repo).

Keeps recipes named "mealier-fixture*" and everything hanging off them, all
foods/units/aliases/labels, fake users. Drops tokens, AI providers, secrets,
share/invite tokens, and image files of other recipes. Any table this script
does not know about must be empty, or it stops. Before writing, it checks the
output for every sensitive value found in the input. Stdlib only.
"""
import json, sys, zipfile
from pathlib import Path

P = "mealier-fixture"
OUT = Path(__file__).resolve().parent.parent / "tests/fixtures/mealie_v3.28.0.zip"

KEEP_ALL = {  # copied as is
    "alembic_version", "groups", "households", "group_preferences", "household_preferences",
    "ingredient_foods", "ingredient_foods_aliases", "ingredient_units", "ingredient_units_aliases",
    "multi_purpose_labels", "households_to_ingredient_foods", "ingredient_food_extras",
    "ingredient_foods_substitutions", "users",
}
DROP = {  # removed entirely
    "long_live_tokens", "ai_providers", "ai_provider_settings", "ai_provider_headers",
    "ai_provider_params", "invite_tokens", "password_reset_tokens", "recipe_share_tokens",
    "webhook_urls", "api_extras", "server_tasks", "report_entries", "group_reports",
    "group_data_exports", "group_events_notifiers", "group_events_notifier_options",
}
BY_RECIPE = {  # rows with a recipe_id column, kept when the recipe is kept
    "recipe_instructions", "recipe_nutrition", "recipe_settings", "recipe_timeline_events",
    "recipe_comments", "recipes_ingredients", "recipes_to_categories", "recipes_to_tags",
    "recipes_to_tools", "households_to_recipes", "users_to_recipes", "recipe_assets",
    "recipe_actions", "notes", "shopping_list_recipe_reference",
}
BY_NAME = {"tags", "categories", "tools", "cookbooks"}  # kept when named "mealier-fixture*"
SENSITIVE_COLS = ("token", "secret", "key", "password")  # users columns dropped (password blanked)


def main():
    src = Path(sys.argv[1])
    out = Path(sys.argv[2]) if len(sys.argv) > 2 else OUT
    zin = zipfile.ZipFile(src)
    db = json.loads(zin.read("database.json"))

    # What must not survive: collected from the input, never printed.
    bad = {"Best Beef Enchiladas"}
    for u in db["users"]:
        bad |= {u.get("email"), u.get("full_name"), u.get("password")}
        if len(u.get("username") or "") > 3:
            bad.add(u["username"])
    for t in db.get("long_live_tokens", []):
        bad.add(t.get("token"))
    for t in db.get("ai_providers", []):
        bad.add(t.get("api_key"))
    bad.discard(None); bad.discard("")

    keep = {r["id"] for r in db["recipes"] if r["name"].startswith(P)}
    out_db = {}
    for t, rows in db.items():
        if t in DROP:
            continue
        if t == "recipes":
            rows = [r for r in rows if r["id"] in keep]
        elif t in BY_RECIPE:
            rows = [r for r in rows if r["recipe_id"] in keep]
        elif t in BY_NAME:
            rows = [r for r in rows if r["name"].startswith(P)]
        elif t == "group_meal_plans":
            rows = [r for r in rows if r["recipe_id"] in keep or (r["title"] or "").startswith(P)]
        elif t == "shopping_lists":
            refd = {r["shopping_list_id"] for r in db["shopping_list_recipe_reference"] if r["recipe_id"] in keep}
            rows = [r for r in rows if r["name"].startswith(P) or r["id"] in refd]
        elif t in KEEP_ALL:
            pass
        elif t in ("cookbooks_to_categories", "cookbooks_to_tags", "cookbooks_to_tools"):
            ids = {r["id"] for r in out_db["cookbooks"]}
            rows = [r for r in rows if r["cookbook_id"] in ids]
        elif t.startswith("shopping_list"):
            pass  # filtered below, once the lists are known
        elif rows:
            sys.exit(f"table {t!r} has {len(rows)} rows and this script does not know it")
        out_db[t] = rows

    lists = {r["id"] for r in out_db["shopping_lists"]}
    items = [r for r in db["shopping_list_items"] if r["shopping_list_id"] in lists]
    item_ids = {r["id"] for r in items}
    out_db["shopping_list_items"] = items
    out_db["shopping_list_item_recipe_reference"] = [
        r for r in db["shopping_list_item_recipe_reference"] if r["shopping_list_item_id"] in item_ids]
    out_db["shopping_list_extras"] = [r for r in db["shopping_list_extras"] if r["shopping_list_id"] in lists]
    out_db["shopping_list_item_extras"] = [r for r in db["shopping_list_item_extras"] if r["shopping_list_item_id"] in item_ids]
    out_db["shopping_lists_multi_purpose_labels"] = [
        r for r in db["shopping_lists_multi_purpose_labels"] if r["shopping_list_id"] in lists]

    for g in out_db["groups"] + out_db["households"]:
        g["name"], g["slug"] = "Fixture", "fixture"
    fakes = ["fixture-admin", "fixture-user"]
    for i, u in enumerate(sorted(out_db["users"], key=lambda u: not u["admin"])):
        name = fakes[i] if i < len(fakes) else f"fixture-user-{i}"
        u["username"], u["full_name"], u["email"] = name, name.replace("-", " ").title(), f"{name}@example.invalid"
        u["password"] = ""
        for c in [c for c in u if any(s in c for s in SENSITIVE_COLS) and c != "password"]:
            del u[c]

    # No column named like a secret anywhere, apart from the blanked users.password.
    for t, rows in out_db.items():
        for c in (rows[0] if rows else {}):
            if any(s in c for s in SENSITIVE_COLS) and (t, c) != ("users", "password"):
                sys.exit(f"sensitive column {t}.{c} survived")

    blob = json.dumps(out_db, sort_keys=True, indent=1)
    names = [n for n in zin.namelist() if n.startswith("data/recipes/")
             and n.split("/")[2].replace("-", "") in keep and not n.endswith("/")]
    for b in bad:
        if b in blob or any(b in n for n in names):
            sys.exit("a sensitive value survived the scrub")

    out.parent.mkdir(parents=True, exist_ok=True)
    stamp = (1980, 1, 1, 0, 0, 0)  # fixed, so reruns are byte-identical
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as zout:
        for name, data in [("database.json", blob.encode())] + [(n, zin.read(n)) for n in sorted(names)]:
            zi = zipfile.ZipInfo(name, stamp)
            zi.compress_type = zipfile.ZIP_DEFLATED
            zout.writestr(zi, data)
    print(f"wrote {out}: {len(keep)} recipes, {len(out_db['users'])} users, {len(names)} image files")


main()
