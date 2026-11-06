#!/usr/bin/env python3 -I
"""Seed production Mealie with additive "mealier-fixture" data, then back it up.
Idempotent: anything whose name already exists is skipped. Stdlib only.
Request shapes (paths, enums, notifier options) are read from the server's /openapi.json.
Usage: python3 -I seed-mealie-fixture.py [--no-backup]"""
import json, os, secrets, struct, sys, time, uuid, zlib
import urllib.error, urllib.parse, urllib.request
from pathlib import Path

BASE = "https://mealie.example.com"
P = "mealier-fixture"
XDG = Path(os.environ.get("XDG_DATA_HOME") or Path.home() / ".local/share")
TOKEN = (XDG / "_r_/mealie/token").read_text().strip()
made = {}


def call(method, path, body=None, raw=None, ctype=None, retries=3, tok=None):
    hdr = {"Authorization": f"Bearer {tok or TOKEN}", "Accept": "application/json"}
    data = raw
    if body is not None:
        data = json.dumps(body).encode()
        ctype = "application/json"
    if ctype:
        hdr["Content-Type"] = ctype
    for i in range(retries):
        try:
            req = urllib.request.Request(BASE + path, data=data, headers=hdr, method=method)
            with urllib.request.urlopen(req, timeout=90) as r:
                b = r.read()
                return json.loads(b) if b and "json" in r.headers.get("content-type", "") else b
        except urllib.error.HTTPError as e:
            if e.code in (502, 503, 504) and i < retries - 1:
                time.sleep(10); continue
            sys.exit(f"{method} {path} -> {e.code}: {e.read()[:300]!r}")
        except (urllib.error.URLError, TimeoutError):
            if i == retries - 1: raise
            time.sleep(10)


def items(path):
    sep = "&" if "?" in path else "?"
    r = call("GET", f"{path}{sep}perPage=-1&page=1")
    return r["items"] if isinstance(r, dict) and "items" in r else r


def ensure(kind, listpath, createpath, body, key="name"):
    for it in items(listpath):
        if it.get(key) == body[key]:
            print(f"skip   {kind}: {body[key]}"); return it
    made[kind] = made.get(kind, 0) + 1
    r = call("POST", createpath, body)
    print(f"create {kind}: {body[key]}")
    return r


def png():  # 8x8 solid-colour RGB PNG
    def ch(t, d): return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d))
    rows = b"".join(b"\0" + bytes([200, 80, 40]) * 8 for _ in range(8))
    return b"\x89PNG\r\n\x1a\n" + ch(b"IHDR", struct.pack(">IIBBBBB", 8, 8, 8, 2, 0, 0, 0)) + ch(b"IDAT", zlib.compress(rows)) + ch(b"IEND", b"")


def multipart(fields, fname, fdata):
    b = "----" + secrets.token_hex(8); out = b""
    for k, v in fields.items():
        out += f'--{b}\r\nContent-Disposition: form-data; name="{k}"\r\n\r\n{v}\r\n'.encode()
    out += f'--{b}\r\nContent-Disposition: form-data; name="image"; filename="{fname}"\r\nContent-Type: image/png\r\n\r\n'.encode() + fdata + f"\r\n--{b}--\r\n".encode()
    return out, f"multipart/form-data; boundary={b}"


def find(path, key, value):
    return next((x for x in items(path) if x.get(key) == value), None)


def login_as(username, email):
    """A JWT for a fixture user: reset their password (random, never stored or printed), then log in."""
    t = call("POST", "/api/admin/users/password-reset-token", {"email": email})["token"]
    pw = secrets.token_urlsafe(24)
    call("POST", "/api/users/reset-password", {"token": t, "email": email, "password": pw, "passwordConfirm": pw})
    form = urllib.parse.urlencode({"username": username, "password": pw}).encode()
    return call("POST", "/api/auth/token", raw=form, ctype="application/x-www-form-urlencoded")["access_token"]


def extend(me, tags, cats, tool, label, cup, pinch, flour, salt, A, recA):
    """Step 16: one of everything Mealie's v3.28 backup can hold, so the importer is tested on real rows.
    Paths, enums and the notifier option names come from /openapi.json."""
    spec = call("GET", "/openapi.json")
    C = spec["components"]["schemas"]
    for need in ("/api/admin/groups", "/api/admin/households", "/api/households/mealplans/rules", "/api/households/recipe-actions",
                 "/api/shared/recipes", "/api/households/events/notifications", "/api/households/webhooks", "/api/recipes/{slug}/assets"):
        assert need in spec["paths"], need
    days, slots = C["PlanRulesDay"]["enum"], C["PlanRulesType"]["enum"]
    actions = C["GroupRecipeActionType"]["enum"]
    option_names = list(C["GroupEventNotifierOptions"]["properties"])
    assert {"link", "post"} <= set(actions) and "monday" in days and "dinner" in slots

    # foods: extras and substitutions (sugar -> honey with a note, and a note-only substitution), flour on hand
    honey = ensure("food", "/api/foods", "/api/foods", {"name": f"{P}-honey"})
    sugar = ensure("food", "/api/foods", "/api/foods", {"name": f"{P}-sugar"})
    sugar = call("GET", f"/api/foods/{sugar['id']}")
    if not sugar.get("substitutions"):
        sugar.update({"extras": {"fixture_food_key": "fixture food value"},
                      "substitutions": [{"substituteFoodId": honey["id"], "note": f"{P} use 3/4 as much"},
                                        {"substituteFoodId": None, "note": f"{P} or any syrup"}]})
        call("PUT", f"/api/foods/{sugar['id']}", sugar); made["food extras+substitutions"] = 1
    fl = call("GET", f"/api/foods/{flour['id']}")
    if not fl.get("householdsWithIngredientFood"):
        fl["householdsWithIngredientFood"] = [me["householdSlug"]]
        call("PUT", f"/api/foods/{fl['id']}", fl); made["food on hand"] = 1
    tl = call("GET", f"/api/organizers/tools/{tool['id']}")
    if not tl.get("householdsWithTool"):
        tl["householdsWithTool"] = [me["householdSlug"]]
        call("PUT", f"/api/organizers/tools/{tool['id']}", tl); made["tool owned"] = 1

    # cookbooks: tags/categories/tools, with "in" and "contains all" variations
    t1, t2, c1, c2 = (x["id"] for x in tags + cats)
    q = lambda f, op, ids: f'{f} {op} [{", ".join(chr(34) + i + chr(34) for i in ids)}]'
    ensure("cookbook", "/api/households/cookbooks", "/api/households/cookbooks",
           {"name": f"{P}-cookbook-any", "description": "tags/categories/tools, any of",
            "queryFilterString": " AND ".join((q("tags.id", "IN", [t1, t2]), q("recipe_category.id", "IN", [c1, c2]), q("tools.id", "IN", [tool["id"]])))})
    ensure("cookbook", "/api/households/cookbooks", "/api/households/cookbooks",
           {"name": f"{P}-cookbook-all", "description": "tags/categories/tools, all of", "public": True, "position": 3,
            "queryFilterString": " AND ".join((q("tags.id", "CONTAINS ALL", [t1, t2]), q("recipe_category.id", "CONTAINS ALL", [c1, c2]), q("tools.id", "CONTAINS ALL", [tool["id"]])))})

    # meal-plan rules: day + slot + tag (its household is the admin's), and an unset one with a category
    rules = items("/api/households/mealplans/rules")
    for body in ({"day": "monday", "entryType": "dinner", "queryFilterString": q("tags.id", "IN", [t1])},
                 {"day": "unset", "entryType": "unset", "queryFilterString": q("recipe_category.id", "IN", [c1, c2])}):
        if not any(r["queryFilterString"] == body["queryFilterString"] for r in rules):
            call("POST", "/api/households/mealplans/rules", body); made["meal-plan rule"] = made.get("meal-plan rule", 0) + 1

    # recipe actions
    for typ, url in (("link", "https://example.invalid/mealier-fixture/link"), ("post", "https://example.invalid/mealier-fixture")):
        ensure("recipe action", "/api/households/recipe-actions", "/api/households/recipe-actions", {"actionType": typ, "title": f"{P}-action-{typ}", "url": url}, key="title")

    # recipe C: everything a recipe can hang off
    Cr = ensure("recipe", "/api/recipes", "/api/recipes", {"name": f"{P}-recipe-c"})
    slugC = Cr if isinstance(Cr, str) else Cr["slug"]
    recC = call("GET", f"/api/recipes/{slugC}")
    if recC.get("description") != "mealier importer fixture C":
        ref = lambda: str(uuid.uuid4())
        r1, r2, r3 = ref(), ref(), ref()
        note_ref = ref()
        def ing(q_, u, f, r, note="", orig="", title=None, subs=()):
            return {"quantity": q_, "unit": u, "food": f, "note": note, "originalText": orig, "display": "", "title": title,
                    "referenceId": r, "substitutions": list(subs)}
        recC.update({
            "description": "mealier importer fixture C", "recipeServings": 2, "orgURL": "https://example.invalid/fixture-c",
            "tags": [tags[1]], "recipeCategory": [cats[0]], "tools": [tool],
            "extras": {"fixture_recipe_key": "fixture recipe value", "fixture_second_key": "2"},
            "notes": [{"title": f"{P} note", "text": "a recipe note", "referenceId": note_ref}],
            "recipeIngredient": [
                ing(2, cup, flour, r1, orig="2 cups flour", title=f"{P} section: dough"),
                ing(1, pinch, sugar, r2, orig="1 pinch sugar",
                    subs=[{"substituteFoodId": honey["id"], "note": f"{P} honey, less"}, {"substituteFoodId": None, "note": f"{P} or maple syrup"}]),
                ing(0, None, None, r3, note="water as needed", orig="water as needed", title=f"{P} section: wet"),
            ],
            "recipeInstructions": [
                {"title": f"{P} step title", "summary": f"{P} summary", "text": "Mix flour and sugar.", "ingredientReferences": [{"referenceId": r1}, {"referenceId": r2}], "noteReferences": [{"referenceId": note_ref}]},
                {"title": None, "summary": None, "text": "Add water.", "ingredientReferences": [{"referenceId": r3}], "noteReferences": []},
            ],
        })
        call("PUT", f"/api/recipes/{slugC}", recC); made["recipe-c-filled"] = 1
    recC = call("GET", f"/api/recipes/{slugC}")
    if not recC["tools"]:  # an earlier run filled C without it; cookbook variations need C to match "any" but not "all"
        call("PATCH", f"/api/recipes/{slugC}", {"tools": [tool]}); made["recipe-c tool"] = 1
    if not recC.get("assets"):
        b = "----" + secrets.token_hex(8)
        parts = "".join(f'--{b}\r\nContent-Disposition: form-data; name="{k}"\r\n\r\n{v}\r\n' for k, v in (("name", f"{P} asset"), ("icon", "mdi-file"), ("extension", "txt")))
        body = parts.encode() + f'--{b}\r\nContent-Disposition: form-data; name="file"; filename="fixture.txt"\r\nContent-Type: text/plain\r\n\r\n'.encode() + b"mealier fixture asset\n" + f"\r\n--{b}--\r\n".encode()
        call("POST", f"/api/recipes/{slugC}/assets", raw=body, ctype=f"multipart/form-data; boundary={b}"); made["asset"] = 1
    if not any(r["name"] == f"{P}-recipe-c" and r.get("rating") for r in items("/api/recipes")):
        call("POST", f"/api/users/{me['id']}/ratings/{slugC}", {"rating": 4, "isFavorite": True}); made["favourite+rating"] = 1
    if not call("GET", f"/api/shared/recipes?recipe_id={recC['id']}"):
        call("POST", "/api/shared/recipes", {"recipeId": recC["id"], "expiresAt": "2036-01-01T00:00:00"}); made["share token"] = 1
    if not recC["settings"]["locked"]:  # last: a locked recipe is not edited again
        call("PATCH", f"/api/recipes/{slugC}", {"settings": {**recC["settings"], "locked": True, "disableComments": True}}); made["recipe settings"] = 1

    # list and item extras on the existing list
    sl = find("/api/households/shopping/lists", "name", f"{P}-list")
    full = call("GET", f"/api/households/shopping/lists/{sl['id']}")
    if not full["extras"]:
        full["extras"] = {"fixture_list_key": "fixture list value"}
        call("PUT", f"/api/households/shopping/lists/{sl['id']}", full); made["list extras"] = 1
        full = call("GET", f"/api/households/shopping/lists/{sl['id']}")
        assert len(full["listItems"]) == 4, "the list update lost items"
    hand = next(i for i in full["listItems"] if i["note"] == "hand added")
    if not hand["extras"]:
        hand["extras"] = {"fixture_item_key": "fixture item value"}
        call("PUT", f"/api/households/shopping/items/{hand['id']}", hand); made["item extras"] = 1

    # second group and household, holding the second user, with a plan entry and a list of its own
    g2 = find("/api/admin/groups", "name", f"{P}-group") or (made.update({"group": 1}) or call("POST", "/api/admin/groups", {"name": f"{P}-group"}))
    h2 = next((h for h in items("/api/admin/households") if h["groupId"] == g2["id"]), None)
    if h2 is None:
        h2 = call("POST", "/api/admin/households", {"name": f"{P}-household", "groupId": g2["id"]}); made["household"] = 1
    elif h2["name"] != f"{P}-household":
        h2 = call("PUT", f"/api/admin/households/{h2['id']}", {"id": h2["id"], "name": f"{P}-household", "groupId": g2["id"], "preferences": h2.get("preferences")}); made["household renamed"] = 1
    u2 = find("/api/admin/users", "username", f"{P}-user")
    if u2["groupId"] != g2["id"]:
        u2.update({"group": g2["name"], "groupId": g2["id"], "household": h2["name"], "householdId": h2["id"]})
        call("PUT", f"/api/admin/users/{u2['id']}", u2); made["user moved"] = 1
        u2 = call("GET", f"/api/admin/users/{u2['id']}")
        assert (u2["groupId"], u2["householdId"]) == (g2["id"], h2["id"]), "the user did not move"
    # their own plan entry and list are only visible to them
    tok = login_as(u2["username"], u2["email"])
    def mine(path):
        r = call("GET", f"{path}?perPage=-1&page=1", tok=tok)
        return r["items"] if isinstance(r, dict) else r
    if not any(m["title"] == f"{P} g2 note entry" for m in mine("/api/households/mealplans")):
        call("POST", "/api/households/mealplans", {"date": "2026-12-05", "entryType": "dinner", "title": f"{P} g2 note entry", "text": "second household", "recipeId": None}, tok=tok); made["g2 mealplan entry"] = 1
    if not any(l["name"] == f"{P}-list-g2" for l in mine("/api/households/shopping/lists")):
        l2 = call("POST", "/api/households/shopping/lists", {"name": f"{P}-list-g2"}, tok=tok)
        call("POST", "/api/households/shopping/items", {"shoppingListId": l2["id"], "quantity": 2, "note": f"{P} g2 item"}, tok=tok); made["g2 list"] = 1

    # last, so nothing above triggers a delivery attempt: scheduled webhook and three notifiers, all fake
    ensure("scheduled webhook", "/api/households/webhooks", "/api/households/webhooks",
           {"enabled": False, "name": f"{P}-webhook", "url": "https://example.invalid/mealier-fixture", "webhookType": "mealplan", "scheduledTime": "07:30"})
    def on(*names):
        assert set(names) <= set(option_names), set(names) - set(option_names)
        return {o: o in names for o in option_names}
    for name, url, enabled, opts in (
        (f"{P}-notifier", "ntfys://ntfy.example.invalid/mealier-fixture", True,
         on("recipeCreated", "recipeUpdated", "mealplanEntryCreated", "shoppingListCreated", "cookbookDeleted", "userSignup", "dataImport", "tagCreated", "labelDeleted")),
        (f"{P}-notifier-json", "json://example.invalid/mealier-fixture", False, on("recipeDeleted", "shoppingListUpdated", "shoppingListDeleted", "cookbookCreated")),
        (f"{P}-notifier-discord", "discord://0000/fixture", True, on("recipeCreated")),
    ):
        n = ensure("notifier", "/api/households/events/notifications", "/api/households/events/notifications", {"name": name, "appriseUrl": url})
        if n.get("options") != opts or n.get("enabled") != enabled:
            n.update({"enabled": enabled, "options": opts, "appriseUrl": url})
            call("PUT", f"/api/households/events/notifications/{n['id']}", n)


def main():
    me = call("GET", "/api/users/self")
    print("authenticated; first call done")

    tags = [ensure("tag", "/api/organizers/tags", "/api/organizers/tags", {"name": f"{P}-tag-{i}"}) for i in (1, 2)]
    cats = [ensure("category", "/api/organizers/categories", "/api/organizers/categories", {"name": f"{P}-cat-{i}"}) for i in (1, 2)]
    tool = ensure("tool", "/api/organizers/tools", "/api/organizers/tools", {"name": f"{P}-tool"})
    label = ensure("label", "/api/groups/labels", "/api/groups/labels", {"name": f"{P}-label", "color": "#336699"})
    ensure("cookbook", "/api/households/cookbooks", "/api/households/cookbooks",
           {"name": f"{P}-cookbook", "description": "fixture", "queryFilterString": f'tags.id IN ["{tags[0]["id"]}"]'})
    cup = ensure("unit", "/api/units", "/api/units", {"name": f"{P}-cup", "abbreviation": f"{P}-c", "fraction": True, "aliases": [{"name": f"{P}-cups"}]})
    pinch = ensure("unit", "/api/units", "/api/units", {"name": f"{P}-pinch", "abbreviation": f"{P}-p"})
    flour = ensure("food", "/api/foods", "/api/foods", {"name": f"{P}-flour", "labelId": label["id"], "aliases": [{"name": f"{P}-flour-alias"}]})
    salt = ensure("food", "/api/foods", "/api/foods", {"name": f"{P}-salt"})

    # recipe A
    A = ensure("recipe", "/api/recipes", "/api/recipes", {"name": f"{P}-recipe-a"})
    if isinstance(A, str):  # POST /api/recipes returns the slug
        A = {"slug": A}
    recA = call("GET", f"/api/recipes/{A['slug']}")
    if recA.get("description") != "mealier importer fixture":  # new recipes ship a placeholder ingredient
        def ing(q, u, f, note="", orig=""):
            return {"quantity": q, "unit": u, "food": f, "note": note, "originalText": orig or None,
                    "display": "", "title": None, "referenceId": str(uuid.uuid4())}
        recA.update({
            "description": "mealier importer fixture", "recipeServings": 4, "recipeYieldQuantity": 4, "recipeYield": "4 servings",
            "prepTime": "PT10M", "cookTime": "PT20M", "totalTime": "PT30M", "orgURL": "https://example.invalid/fixture",
            "tags": tags, "recipeCategory": cats, "tools": [tool],
            "nutrition": {"calories": "250", "carbohydrateContent": "30", "fatContent": "10", "proteinContent": "5",
                          "fiberContent": "2", "sodiumContent": "300", "sugarContent": "4"},
            "settings": {"public": False, "showNutrition": True, "showAssets": False, "landscapeView": False, "disableComments": False, "locked": False},
            "recipeIngredient": [
                ing(2, cup, flour, orig="2 cups flour"),
                ing(0.5, cup, flour, orig="1/2 cup flour"),
                ing(0, None, salt, note="to taste", orig="salt, to taste"),
                ing(0, None, None, note="a handful of fresh herbs, roughly torn", orig="a handful of fresh herbs, roughly torn"),
                ing(1, pinch, salt, note="finely ground", orig="1 pinch salt, finely ground"),
                ing(1.5, cup, flour, note="range 1-2 cups in source; schema has no range", orig="1-2 cups flour"),
            ],
            "recipeInstructions": [{"title": None, "summary": None, "text": t, "ingredientReferences": []} for t in
                                   ("Mix the dry ingredients.", "Add water and knead.", "Bake until golden.")],
        })
        call("PUT", f"/api/recipes/{A['slug']}", recA)
        made["recipe-a-filled"] = 1
        body, ct = multipart({"extension": "png"}, "fixture.png", png())
        call("PUT", f"/api/recipes/{A['slug']}/image", raw=body, ctype=ct)
        print("filled recipe A + image")
    recA = call("GET", f"/api/recipes/{A['slug']}")

    # recipe B (unparsed)
    B = ensure("recipe", "/api/recipes", "/api/recipes", {"name": f"{P}-recipe-b"})
    slugB = B if isinstance(B, str) else B["slug"]
    recB = call("GET", f"/api/recipes/{slugB}")
    if not any(i.get("note") == "3 eggs, beaten" for i in recB["recipeIngredient"]):
        recB["recipeIngredient"] = [{"quantity": 0, "unit": None, "food": None, "note": n, "originalText": n, "display": "",
                                     "title": None, "referenceId": str(uuid.uuid4())} for n in ("3 eggs, beaten", "a splash of milk")]
        recB["recipeInstructions"] = [{"title": None, "summary": None, "text": "Whisk and fry.", "ingredientReferences": []}]
        call("PUT", f"/api/recipes/{slugB}", recB)
        made["recipe-b-filled"] = 1

    rid = recA["id"]
    # timeline events + comment (only if recipe has none of ours)
    ev = call("GET", f"/api/recipes/timeline/events?perPage=-1&queryFilter={urllib.parse.quote(f'recipeId = \"{rid}\"')}")["items"]
    if not any(e["subject"].startswith(P) for e in ev):
        now = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime())
        for subj, typ, msg in ((f"{P} made it", "comment", "made it, tasted good"), (f"{P} info event", "info", "plain info event")):
            call("POST", "/api/recipes/timeline/events", {"recipeId": rid, "subject": subj, "eventType": typ, "eventMessage": msg, "timestamp": now})
            made["timeline event"] = made.get("timeline event", 0) + 1
        call("PATCH", f"/api/recipes/{A['slug']}/last-made", {"timestamp": now})
    if not any(c["text"].startswith(P) for c in call("GET", f"/api/recipes/{A['slug']}/comments")):
        call("POST", "/api/comments", {"recipeId": rid, "text": f"{P} comment"}); made["comment"] = 1

    # meal plan: entryType has no "note" value; note-only = no recipeId, title+text
    have = {m["title"] for m in items("/api/households/mealplans") if m.get("title")}
    for d, body in (("2026-12-01", {"entryType": "dinner", "title": f"{P} planned recipe", "text": "", "recipeId": rid}),
                    ("2026-12-02", {"entryType": "lunch", "title": f"{P} note entry", "text": "eat out, no recipe", "recipeId": None})):
        if body["title"] not in have:
            call("POST", "/api/households/mealplans", {"date": d, **body}); made["mealplan entry"] = made.get("mealplan entry", 0) + 1

    # shopping list
    lists = items("/api/households/shopping/lists")
    sl = next((l for l in lists if l["name"] == f"{P}-list"), None)
    if sl is None:
        sl = call("POST", "/api/households/shopping/lists", {"name": f"{P}-list"}); made["shopping list"] = 1
        sid = sl["id"]
        call("POST", f"/api/households/shopping/lists/{sid}/recipe/{rid}", {"recipeIncrementQuantity": 1})
        mk = lambda **k: {"shoppingListId": sid, "quantity": 1, **k}
        for it in (mk(quantity=3, unitId=cup["id"], foodId=flour["id"], note="hand added"),
                   mk(note=f"{P} checked item", checked=True),
                   mk(quantity=0, note=f"{P} note-only item")):
            call("POST", "/api/households/shopping/items", it)
        made["shopping items (hand)"] = 3
    else:
        print(f"skip   shopping list: {P}-list")

    # second user
    if not any(u["username"] == f"{P}-user" for u in items("/api/admin/users")):
        try:
            call("POST", "/api/admin/users", {"username": f"{P}-user", "fullName": f"{P} user", "email": f"{P}@example.invalid",
                 "password": secrets.token_urlsafe(24), "admin": False, "group": me["group"], "household": me["household"]}, retries=1)
            made["user"] = 1
        except SystemExit as e:
            print("user creation failed:", e)

    extend(me, tags, cats, tool, label, cup, pinch, flour, salt, A, recA)

    print("created:", made)
    if "--no-backup" in sys.argv: return
    call("POST", "/api/admin/backups")
    names = [b["name"] for b in call("GET", "/api/admin/backups")["imports"]]
    newest = max(names, key=lambda n: next(b["date"] for b in call("GET", "/api/admin/backups")["imports"] if b["name"] == n))
    tok = call("GET", f"/api/admin/backups/{urllib.parse.quote(newest)}")["fileToken"]
    data = call("GET", f"/api/utils/download?token={tok}")
    out = XDG / "_r_/mealier/fixtures"; out.mkdir(parents=True, exist_ok=True)
    dest = out / newest
    fd = os.open(dest, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600); os.write(fd, data); os.close(fd)
    print("backup:", dest, len(data))


main()
