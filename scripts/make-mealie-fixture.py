#!/usr/bin/env python3 -I
"""Scrub a Mealie v3.28 backup zip into the committed test fixture.

Usage: python3 -I scripts/make-mealie-fixture.py INPUT.zip [OUTPUT.zip]
OUTPUT defaults to tests/fixtures/mealie_v3.28.0.zip (relative to the repo).

Keeps recipes named "mealier-fixture*" and everything hanging off them, all
foods/units/aliases/labels, fake users, and the fixture's notifiers, scheduled
webhook, recipe actions, meal-plan rules and share token. Drops API tokens, AI
providers, secrets, invite and password-reset tokens, and image files of other
recipes. The share token's value (a read link to a fixture recipe on production)
is replaced by a fake one; notifier and webhook URLs must already be fakes, or it
stops. Any table this script does not know about must be empty, or it stops.
Before writing, it checks the output for every sensitive value found in the
input. Stdlib only.
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
    "ai_provider_params", "invite_tokens", "password_reset_tokens",
    "server_tasks", "report_entries", "group_reports", "group_data_exports",
}
AFTER = {"recipe_ingredient_ref_link", "recipe_note_ref_link", "recipes_ingredients_substitutions", "households_to_tools",
         "group_events_notifier_options"}
FAKE_SHARE_TOKEN = "fa4e" * 8  # replaces the real one: 32 hex, the form Mealie stores
FAKE_URLS = ("ntfys://ntfy.example.invalid/", "json://example.invalid/", "discord://0000/", "https://example.invalid/")
BY_RECIPE = {  # rows with a recipe_id column, kept when the recipe is kept
    "recipe_instructions", "recipe_nutrition", "recipe_settings", "recipe_timeline_events",
    "recipe_comments", "recipes_ingredients", "recipes_to_categories", "recipes_to_tags",
    "recipes_to_tools", "households_to_recipes", "users_to_recipes", "recipe_assets",
    "notes", "shopping_list_recipe_reference", "recipe_share_tokens",
}
# kept when named "mealier-fixture*" (title for recipe actions); meal-plan rules have no name: kept if
# their filter text mentions a kept tag or category, see main()
BY_NAME = {"tags", "categories", "tools", "cookbooks", "group_events_notifiers", "webhook_urls"}
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
    for t in db.get("recipe_share_tokens", []):
        bad |= {t["id"], f"{t['id'][:8]}-{t['id'][8:12]}-{t['id'][12:16]}-{t['id'][16:20]}-{t['id'][20:]}"}
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
        elif t == "recipe_actions":
            rows = [r for r in rows if r["title"].startswith(P)]
        elif t == "api_extras":
            rows = [r for r in rows if r["recipee_id"] in keep]
        elif t in AFTER:
            continue  # filtered below, once what they hang off is known
        elif t in ("group_meal_plan_rules", "plan_rules_to_tags", "plan_rules_to_categories", "plan_rules_to_households"):
            pass  # rules are filtered below, once the tags and categories are known
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

    for t in sorted(AFTER):
        rows = db[t]
        if t in ("recipe_ingredient_ref_link", "recipe_note_ref_link"):
            ids = {r["id"] for r in out_db["recipe_instructions"]}
            rows = [r for r in rows if r["instruction_id"] in ids]
        elif t == "recipes_ingredients_substitutions":
            ids = {r["id"] for r in out_db["recipes_ingredients"]}
            rows = [r for r in rows if r["ingredient_id"] in ids]
        elif t == "households_to_tools":
            ids = {r["id"] for r in out_db["tools"]}
            rows = [r for r in rows if r["tool_id"] in ids]
        else:
            ids = {r["id"] for r in out_db["group_events_notifiers"]}
            rows = [r for r in rows if r["event_notifier_id"] in ids]
        out_db[t] = rows

    # Meal-plan rules: a fixture's rule is one whose filter names a kept tag or category.
    named = {r["id"] for t in ("tags", "categories") for r in out_db[t]}
    dashed = lambda i: f"{i[:8]}-{i[8:12]}-{i[12:16]}-{i[16:20]}-{i[20:]}"
    out_db["group_meal_plan_rules"] = [r for r in db["group_meal_plan_rules"] if any(dashed(i) in r["query_filter_string"] for i in named)]
    rule_ids = {r["id"] for r in out_db["group_meal_plan_rules"]}
    for t in ("plan_rules_to_tags", "plan_rules_to_categories", "plan_rules_to_households"):
        out_db[t] = [r for r in db[t] if (r.get("group_plan_rule_id") or r.get("plan_rule_id")) in rule_ids]

    # The share token: its id is the secret. Same fake everywhere it could appear.
    real_tokens = {r["id"] for r in db["recipe_share_tokens"]}
    for r in out_db["recipe_share_tokens"]:
        r["id"] = FAKE_SHARE_TOKEN
    if len(out_db["recipe_share_tokens"]) > 1:
        sys.exit("more than one share token kept: they would all get the same fake id")

    for r in out_db["group_events_notifiers"]:
        if not r["apprise_url"].startswith(FAKE_URLS):
            sys.exit(f"notifier {r['name']!r} has an address that is not a known fake")
    for r in out_db["webhook_urls"]:
        if not r["url"].startswith(FAKE_URLS):
            sys.exit(f"webhook {r['name']!r} has an address that is not a known fake")

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

    # The admin's group and household keep "Fixture"; the second group (and its household) is "Fixture Two".
    admin_group = next(u["group_id"] for u in out_db["users"] if u["admin"])
    for g in out_db["groups"] + out_db["households"]:
        first = g["id"] == admin_group or g.get("group_id") == admin_group
        g["name"], g["slug"] = ("Fixture", "fixture") if first else ("Fixture Two", "fixture-two")
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
            if any(s in c for s in SENSITIVE_COLS) and (t, c) != ("users", "password") and c != "key_name":  # key_name: extras
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
