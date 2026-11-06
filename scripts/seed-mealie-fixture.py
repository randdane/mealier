#!/usr/bin/env python3 -I
"""Seed production Mealie with additive "mealier-fixture" data, then back it up.
Idempotent: anything whose name already exists is skipped. Stdlib only.
Usage: python3 -I seed-mealie-fixture.py [--no-backup]"""
import json, os, secrets, struct, sys, time, uuid, zlib
import urllib.error, urllib.parse, urllib.request
from pathlib import Path

BASE = "https://mealie.example.com"
P = "mealier-fixture"
XDG = Path(os.environ.get("XDG_DATA_HOME") or Path.home() / ".local/share")
TOKEN = (XDG / "_r_/mealie/token").read_text().strip()
made = {}


def call(method, path, body=None, raw=None, ctype=None, retries=3):
    hdr = {"Authorization": f"Bearer {TOKEN}", "Accept": "application/json"}
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
