//! S36: through the router, a user in one tenant cannot read or change another tenant's records, on
//! every route that takes an id (404, never 403, so existence does not leak). S37 (HTTP half): the
//! admin-only household management.
mod common;
use std::collections::BTreeSet;
use std::path::PathBuf;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use mealier::{AuthConfig, Db, Dimension, EntryInput, ImportService, IngredientInput, ItemInput, LabelInput, RecipeInput, api, hash_password};
use serde_json::{Value, json};
use tower::ServiceExt;

enum Req {
    None,
    Json(Value),
    Form(String),
    Photo,
}

async fn send(app: &Router, method: &str, uri: &str, bearer: Option<&str>, body: Req) -> (StatusCode, HeaderMap, String) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(b) = bearer {
        req = req.header("authorization", b);
    }
    let body = match body {
        Req::None => Body::empty(),
        Req::Json(v) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        Req::Form(f) => {
            req = req.header("content-type", "application/x-www-form-urlencoded");
            Body::from(f)
        }
        Req::Photo => {
            req = req.header("content-type", "multipart/form-data; boundary=XX");
            let mut b = b"--XX\r\nContent-Disposition: form-data; name=\"photo\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n".to_vec();
            b.extend_from_slice(&common::png(4, 4, 1));
            b.extend_from_slice(b"\r\n--XX--\r\n");
            Body::from(b)
        }
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let (status, h) = (resp.status(), resp.headers().clone());
    (status, h, String::from_utf8_lossy(&resp.into_body().collect().await.unwrap().to_bytes()).into_owned())
}

async fn json_of(app: &Router, method: &str, uri: &str, bearer: &str, body: Req) -> (StatusCode, Value) {
    let (s, _, b) = send(app, method, uri, Some(bearer), body).await;
    (s, serde_json::from_str(&b).unwrap_or(Value::Null))
}

async fn user_in(db: &Db, name: &str, role: &str, household_id: i64) -> (i64, String) {
    let id: i64 = sqlx::query_scalar("INSERT INTO users (username, email, role, household_id) VALUES (?, ?, ?, ?) RETURNING id")
        .bind(name)
        .bind(format!("{name}@x"))
        .bind(role)
        .bind(household_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    let secret = format!("mlr_{name}-token");
    sqlx::query("INSERT INTO api_tokens (hash, user_id, name) VALUES (?, ?, 't')").bind(mealier::hash_token(&secret)).bind(id).execute(db.pool()).await.unwrap();
    (id, format!("Bearer {secret}"))
}

struct World {
    app: Router,
    db: Db,
    t: common::Tenants,
    images: PathBuf,
    /// alice is in household Alpha (group One), bob in Beta (group One), carol in Gamma (group Two).
    alice: String,
    bob: String,
    carol: String,
    ids: Ids,
}

struct Ids {
    recipe: i64,
    snapshot: i64,
    batch: i64,
    line: i64,
    entry: i64,
    list: i64,
    item: i64,
    unit: i64,
    food: i64,
    label: i64,
}

async fn world() -> World {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let (_, alice) = user_in(&db, "alice", "user", t.a.household_id).await;
    let (_, bob) = user_in(&db, "bob", "user", t.b.household_id).await;
    let (_, carol) = user_in(&db, "carol", "user", t.c.household_id).await;
    let unit = db.add_unit(&t.a, "g", Dimension::Mass, Some(1.0)).await.unwrap();
    let food = db.add_food(&t.a, "flour").await.unwrap();
    let recipe = db
        .create_recipe(&t.a, RecipeInput { name: "Pancakes".into(), ingredients: vec![IngredientInput { qty_min: Some(250.0), qty_max: Some(250.0), unit_id: Some(unit), food_id: Some(food), ..Default::default() }], ..Default::default() })
        .await
        .unwrap();
    let b = db.start_batch(&t.a, recipe.id, 1.0).await.unwrap();
    let entry = db.add_entry(&t.a, EntryInput { date: "2026-12-01".into(), slot: "dinner".into(), recipe_id: Some(recipe.id), text: "private-plan-note".into(), ..Default::default() }).await.unwrap();
    let list = db.create_list(&t.a, "Weekly-secret").await.unwrap();
    let item = db.add_item(&t.a, list.id, ItemInput { note: "milk".into(), ..Default::default() }).await.unwrap();
    let label = db.add_label(&t.a, LabelInput { name: "Produce".into(), color: "#00aa00".into() }).await.unwrap();
    let images = tempfile::tempdir().unwrap().keep();
    let app = api::router(api::AppState { db: db.clone(), images_dir: images.clone() });
    let ids = Ids { recipe: recipe.id, snapshot: recipe.current_snapshot_id, batch: b.batch.id, line: b.lines[0].id, entry: entry.id, list: list.id, item: item.id, unit, food, label: label.id };
    World { app, db, t, images, alice, bob, carol, ids }
}

impl World {
    fn fill(&self, s: &str) -> String {
        let i = &self.ids;
        s.replace("{recipe}", &i.recipe.to_string())
            .replace("{snapshot}", &i.snapshot.to_string())
            .replace("{batch}", &i.batch.to_string())
            .replace("{line}", &i.line.to_string())
            .replace("{entry}", &i.entry.to_string())
            .replace("{list}", &i.list.to_string())
            .replace("{item}", &i.item.to_string())
            .replace("{food}", &i.food.to_string())
            .replace("{label}", &i.label.to_string())
    }

    /// Everything the tenants own, as text: if an outsider's request changed anything, this changes.
    async fn fingerprint(&self) -> String {
        let mut out = String::new();
        for sql in [
            "SELECT json_group_array(json_array(id, group_id, name, description, archived, current_snapshot_id)) FROM recipes",
            "SELECT json_group_array(json_array(id, recipe_id, content)) FROM recipe_snapshots",
            "SELECT json_group_array(json_array(id, household_id, status, recipe_id, snapshot_id, made_on, scale, rating, notes)) FROM batches",
            "SELECT json_group_array(json_array(id, batch_id, qty_min, qty_max, unit_id, food_id, substitution, added)) FROM batch_lines",
            "SELECT json_group_array(json_array(batch_id, hash)) FROM batch_photos",
            "SELECT json_group_array(json_array(id, household_id, date, slot, recipe_id, title, text, batch_id)) FROM meal_plan_entries",
            "SELECT json_group_array(json_array(id, household_id, name)) FROM shopping_lists",
            "SELECT json_group_array(json_array(id, list_id, qty, unit_id, food_id, note, checked, recipe_id)) FROM shopping_list_items",
            "SELECT json_group_array(json_array(id, group_id, name)) FROM units",
            "SELECT json_group_array(json_array(id, group_id, name, label_id)) FROM foods",
            "SELECT json_group_array(json_array(id, group_id, name, color)) FROM labels",
            "SELECT json_group_array(json_array(list_id, label_id, position)) FROM shopping_list_labels",
        ] {
            let s: String = sqlx::query_scalar(sql).fetch_one(self.db.pool()).await.unwrap();
            out.push_str(&s);
        }
        out
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    /// Shared by the households of a group: only another group is refused.
    Group,
    /// One household's own: another household of the same group is refused too.
    Household,
    /// Group-wide, and the request removes it: the first household of the group to ask succeeds, the next finds it gone.
    Spent,
}
use Kind::{Group, Household, Spent};

struct Route {
    method: &'static str,
    path: &'static str,
    kind: Kind,
    body: fn(&Ids) -> Req,
    ok: u16,
}

fn none(_: &Ids) -> Req {
    Req::None
}

/// Every /api route that takes an id in its path and works on tenant data.
fn api_routes() -> Vec<Route> {
    let r = |method, path, kind, body, ok| Route { method, path, kind, body, ok };
    vec![
        r("GET", "/api/recipes/{recipe}", Group, none, 200),
        r("PUT", "/api/recipes/{recipe}", Group, |_| Req::Json(json!({"name": "Edited"})), 200),
        r("POST", "/api/recipes/{recipe}/archive", Group, none, 204),
        r("GET", "/api/recipes/{recipe}/snapshots", Group, none, 200),
        r("GET", "/api/snapshots/{snapshot}", Group, none, 200),
        r("POST", "/api/recipes/{recipe}/batches", Group, |_| Req::Json(json!({"scale": 1})), 201),
        r("GET", "/api/recipes/{recipe}/batches", Group, none, 200),
        r("POST", "/api/recipes/{recipe}/nutrition/estimate", Group, none, 422),
        r("GET", "/api/batches/{batch}", Household, none, 200),
        r("PUT", "/api/batches/{batch}", Household, |i| Req::Json(json!({"scale": 1, "notes": "n", "lines": [{"id": i.line, "qty_min": 250, "qty_max": 250}]})), 200),
        r("POST", "/api/batches/{batch}/lines", Household, |_| Req::Json(json!({"qty_min": 1})), 201),
        r("POST", "/api/batches/{batch}/finish", Household, none, 200),
        r("POST", "/api/batches/{batch}/photos", Household, |_| Req::Photo, 201),
        r("PUT", "/api/meal-plan/{entry}", Household, |_| Req::Json(json!({"date": "2026-12-02", "slot": "lunch", "title": "x"})), 200),
        r("DELETE", "/api/meal-plan/{entry}", Household, none, 204),
        r("GET", "/api/shopping-lists/{list}", Household, none, 200),
        r("POST", "/api/shopping-lists/{list}/items", Household, |_| Req::Json(json!({"note": "eggs"})), 201),
        r("PUT", "/api/shopping-list-items/{item}", Household, |_| Req::Json(json!({"note": "x"})), 200),
        r("DELETE", "/api/shopping-list-items/{item}", Household, none, 204),
        r("POST", "/api/shopping-lists/{list}/recipes", Household, |i| Req::Json(json!({"recipe_id": i.recipe, "scale": 1})), 200),
        r("POST", "/api/shopping-lists/{list}/batches", Household, |i| Req::Json(json!({"batch_id": i.batch})), 200),
        r("PUT", "/api/labels/{label}", Group, |_| Req::Json(json!({"name": "Renamed", "color": "#010203"})), 200),
        r("DELETE", "/api/labels/{label}", Spent, none, 204),
        r("PUT", "/api/foods/{food}/label", Group, |_| Req::Json(json!({"label_id": null})), 204),
        r("PUT", "/api/shopping-lists/{list}/section-order", Household, |i| Req::Json(json!({"label_ids": [i.label]})), 200),
    ]
}

/// Every HTML route that takes an id in its path and works on tenant data (a hand list; `html_routes_are_all_listed` checks it against the source).
fn html_routes() -> Vec<Route> {
    let r = |method, path, kind, body, ok| Route { method, path, kind, body, ok };
    let batch_form = |i: &Ids| Req::Form(format!("scale=1&line_id={}&qty_min=250&qty_max=250", i.line));
    vec![
        r("GET", "/recipes/{recipe}", Group, none, 200),
        r("GET", "/recipes/{recipe}/edit", Group, none, 200),
        r("POST", "/recipes/{recipe}/edit", Group, |_| Req::Form("name=Edited&ingredients=".into()), 303),
        r("POST", "/recipes/{recipe}/nutrition/estimate", Group, none, 422),
        r("GET", "/recipes/{recipe}/ingredients/review", Group, none, 200),
        r("POST", "/recipes/{recipe}/ingredients/review", Group, |_| Req::Form(String::new()), 303),
        r("GET", "/recipes/{recipe}/cook", Group, none, 200),
        r("GET", "/recipes/{recipe}/cook?batch={batch}", Household, none, 200),
        r("POST", "/recipes/{recipe}/batches", Group, |_| Req::Form("scale=1".into()), 303),
        r("GET", "/batches/{batch}", Household, none, 200),
        r("POST", "/batches/{batch}", Household, batch_form, 303),
        r("POST", "/batches/{batch}/lines", Household, batch_form, 303),
        r("POST", "/batches/{batch}/done", Household, batch_form, 303),
        r("POST", "/batches/{batch}/photo", Household, |_| Req::Photo, 303),
        r("POST", "/plan/entries/{entry}/delete", Household, |_| Req::Form("week=2026-11-30".into()), 303),
        r("GET", "/lists/{list}", Household, none, 200),
        r("POST", "/lists/{list}/items", Household, |_| Req::Form("item=milk&qty=1".into()), 303),
        r("POST", "/lists/items/{item}/check", Household, |_| Req::Form("checked=on".into()), 303),
        r("POST", "/lists/items/{item}/delete", Household, |_| Req::Form(String::new()), 303),
        r("POST", "/lists/{list}/sections/{label}/move", Household, |_| Req::Form("dir=up".into()), 303),
        r("POST", "/settings/labels/{label}", Group, |_| Req::Form("name=Renamed&color=%23010203".into()), 303),
        r("POST", "/settings/labels/{label}/delete", Spent, |_| Req::Form(String::new()), 303),
    ]
}

async fn check_route(r: &Route) {
    let w = world().await;
    let uri = w.fill(r.path);
    let (method, name) = (r.method, format!("{} {}", r.method, r.path));

    // another group: always refused, and nothing changes
    let before = w.fingerprint().await;
    let (s, _, text) = send(&w.app, method, &uri, Some(&w.carol), (r.body)(&w.ids)).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{name} as another group: {text}");
    assert_eq!(w.fingerprint().await, before, "{name} as another group changed something");

    // another household of the same group
    let (s, _, text) = send(&w.app, method, &uri, Some(&w.bob), (r.body)(&w.ids)).await;
    match r.kind {
        Household => {
            assert_eq!(s, StatusCode::NOT_FOUND, "{name} as another household: {text}");
            assert_eq!(w.fingerprint().await, before, "{name} as another household changed something");
        }
        Group | Spent => assert_eq!(s.as_u16(), r.ok, "{name}: a household of the same group shares it: {text}"),
    }
    if r.kind == Spent {
        let (s, _, text) = send(&w.app, method, &uri, Some(&w.alice), (r.body)(&w.ids)).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{name}: already removed by the other household: {text}");
        return;
    }

    // the owner: proves the route and body are right, so the 404s above were about the tenant
    let (s, _, text) = send(&w.app, method, &uri, Some(&w.alice), (r.body)(&w.ids)).await;
    assert_eq!(s.as_u16(), r.ok, "{name} as the owner: {text}");
}

#[tokio::test]
async fn every_api_route_with_an_id_refuses_other_tenants_with_404() {
    for r in api_routes() {
        check_route(&r).await;
    }
}

#[tokio::test]
async fn every_html_route_with_an_id_refuses_other_tenants_with_404() {
    for r in html_routes() {
        check_route(&r).await;
    }
}

fn normalised(path: &str) -> String {
    let mut out = String::new();
    let mut rest = path.split('?').next().unwrap();
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        out.push_str("{id}");
        rest = &rest[rest.find('}').unwrap() + 1..];
    }
    out.push_str(rest);
    out
}

#[tokio::test]
async fn api_routes_are_all_listed() {
    let w = world().await;
    let (_, spec) = json_of(&w.app, "GET", "/api/openapi.json", &w.alice, Req::None).await;
    let mut documented = BTreeSet::new();
    for (path, item) in spec["paths"].as_object().unwrap() {
        for m in ["get", "post", "put", "patch", "delete"] {
            if item.get(m).is_some() && path.contains('{') {
                documented.insert(format!("{} {}", m.to_uppercase(), path));
            }
        }
    }
    // instance-wide or admin routes: their ids are not tenant data
    let instance: BTreeSet<String> = ["GET /api/webhooks/{id}", "PUT /api/webhooks/{id}", "DELETE /api/webhooks/{id}", "POST /api/webhooks/{id}/test", "POST /api/users/{id}/disable", "POST /api/users/{id}/enable", "PUT /api/users/{id}/household"].map(String::from).into();
    let covered: BTreeSet<String> = api_routes().iter().map(|r| format!("{} {}", r.method, normalised(r.path))).collect();
    let expected: BTreeSet<String> = documented.difference(&instance).cloned().collect();
    assert_eq!(covered, expected, "a route with an id is missing from (or extra in) the table above; add it, or add it to the instance-wide list if its ids are not tenant data");
    assert!(documented.is_superset(&instance), "the instance-wide list names routes that do not exist: {:?}", instance.difference(&documented).collect::<Vec<_>>());
}

#[test]
fn html_routes_are_all_listed() {
    // The page modules together; src/web.rs was split by page area.
    let src = [
        include_str!("../src/web/mod.rs"),
        include_str!("../src/web/recipes.rs"),
        include_str!("../src/web/batches.rs"),
        include_str!("../src/web/planner.rs"),
        include_str!("../src/web/shopping.rs"),
        include_str!("../src/web/settings.rs"),
    ]
    .join("\n");
    let mut in_source = BTreeSet::new();
    for chunk in src.split(".route(").skip(1) {
        let chunk = chunk.trim_start();
        let Some(rest) = chunk.strip_prefix('"') else { continue };
        let path = &rest[..rest.find('"').unwrap()];
        if path.contains('{') {
            in_source.insert(normalised(path));
        }
    }
    // admin-only webhook pages, and /images/{name} (own test)
    let elsewhere: BTreeSet<String> = ["/settings/webhooks/{id}/delete", "/settings/webhooks/{id}/toggle", "/settings/webhooks/{id}/test", "/images/{id}"].map(String::from).into();
    let listed: BTreeSet<String> = html_routes().iter().map(|r| normalised(r.path)).collect();
    let expected: BTreeSet<String> = in_source.difference(&elsewhere).cloned().collect();
    assert_eq!(listed, expected, "src/web/ has a route with an id that the table does not cover (or the table lists one that is gone)");
    assert!(in_source.is_superset(&elsewhere), "stale exemption");
}

// ---------- ids inside bodies ----------

#[tokio::test]
async fn ids_in_request_bodies_are_checked_against_the_callers_tenant() {
    let w = world().await;
    let c_unit = w.db.add_unit(&w.t.c, "cup", Dimension::Volume, Some(236.0)).await.unwrap();
    let c_food = w.db.add_food(&w.t.c, "milk").await.unwrap();
    let c_recipe = w.db.create_recipe(&w.t.c, RecipeInput { name: "Theirs".into(), ..Default::default() }).await.unwrap().id;
    let c_list = w.db.create_list(&w.t.c, "C list").await.unwrap().id;
    let b_list = w.db.create_list(&w.t.b, "B list").await.unwrap().id;
    let c_label = w.db.add_label(&w.t.c, LabelInput { name: "Theirs".into(), color: "#123456".into() }).await.unwrap().id;
    let i = &w.ids;
    let nf = StatusCode::NOT_FOUND;
    let cases: Vec<(&str, &str, String, String, Value)> = vec![
        ("carol", "POST", "/api/meal-plan".to_string(), "recipe of another group".into(), json!({"date": "2026-12-01", "slot": "dinner", "recipe_id": i.recipe})),
        ("bob", "POST", "/api/meal-plan".to_string(), "batch of another household".into(), json!({"date": "2026-12-01", "slot": "dinner", "recipe_id": i.recipe, "batch_id": i.batch})),
        ("alice", "POST", "/api/meal-plan".to_string(), "recipe of another group".into(), json!({"date": "2026-12-01", "slot": "dinner", "recipe_id": c_recipe})),
        ("alice", "PUT", format!("/api/meal-plan/{}", i.entry), "recipe of another group".into(), json!({"date": "2026-12-01", "slot": "dinner", "recipe_id": c_recipe})),
        ("carol", "POST", "/api/recipes".to_string(), "unit of another group".into(), json!({"name": "x", "ingredients": [{"unit_id": i.unit}]})),
        ("carol", "POST", "/api/recipes".to_string(), "food of another group".into(), json!({"name": "x", "ingredients": [{"food_id": i.food}]})),
        ("alice", "PUT", format!("/api/recipes/{}", i.recipe), "unit of another group".into(), json!({"name": "x", "ingredients": [{"unit_id": c_unit}]})),
        ("alice", "POST", format!("/api/batches/{}/lines", i.batch), "unit of another group".into(), json!({"unit_id": c_unit})),
        ("alice", "POST", format!("/api/batches/{}/lines", i.batch), "food of another group".into(), json!({"food_id": c_food})),
        ("alice", "POST", format!("/api/batches/{}/lines", i.batch), "substitute of another group".into(), json!({"substitute_food_id": c_food})),
        ("alice", "PUT", format!("/api/batches/{}", i.batch), "unit of another group".into(), json!({"scale": 1, "lines": [{"id": i.line, "unit_id": c_unit}]})),
        ("alice", "POST", format!("/api/shopping-lists/{}/items", i.list), "unit of another group".into(), json!({"note": "x", "unit_id": c_unit})),
        ("alice", "PUT", format!("/api/shopping-list-items/{}", i.item), "food of another group".into(), json!({"food_id": c_food})),
        ("alice", "POST", format!("/api/shopping-lists/{}/recipes", i.list), "recipe of another group".into(), json!({"recipe_id": c_recipe, "scale": 1})),
        ("bob", "POST", format!("/api/shopping-lists/{b_list}/batches"), "batch of another household".into(), json!({"batch_id": i.batch})),
        ("carol", "POST", format!("/api/shopping-lists/{c_list}/recipes"), "recipe of another group".into(), json!({"recipe_id": i.recipe, "scale": 1})),
        ("alice", "PUT", format!("/api/foods/{}/label", i.food), "label of another group".into(), json!({"label_id": c_label})),
        ("alice", "PUT", format!("/api/shopping-lists/{}/section-order", i.list), "label of another group".into(), json!({"label_ids": [c_label]})),
        ("alice", "POST", format!("/api/shopping-lists/{}/items", i.list), "label of another group".into(), json!({"note": "x", "label_id": c_label})),
        ("alice", "PUT", format!("/api/shopping-list-items/{}", i.item), "label of another group".into(), json!({"note": "x", "label_id": c_label})),
    ];
    for (who, method, uri, what, body) in cases {
        let token = match who {
            "alice" => &w.alice,
            "bob" => &w.bob,
            _ => &w.carol,
        };
        let before = w.fingerprint().await;
        let (s, v) = json_of(&w.app, method, &uri, token, Req::Json(body)).await;
        assert_eq!(s, nf, "{who} {method} {uri} with a {what}: {v}");
        assert_eq!(w.fingerprint().await, before, "{who} {method} {uri} with a {what} changed something");
    }

    // the same ids in HTML forms
    let forms = [
        ("bob", "/lists/add-batch", format!("list_id={b_list}&batch_id={}", i.batch)),
        ("carol", "/lists/add-recipe", format!("list_id={c_list}&recipe_id={}", i.recipe)),
        ("carol", "/plan/entries", format!("date=2026-12-01&slot=dinner&recipe_id={}", i.recipe)),
        ("carol", "/plan/entries", format!("date=2026-12-01&slot=dinner&recipe_id={}&scale=1", i.recipe)),
    ];
    for (who, uri, form) in forms {
        let token = if who == "bob" { &w.bob } else { &w.carol };
        let before = w.fingerprint().await;
        let (s, _, text) = send(&w.app, "POST", uri, Some(token), Req::Form(form.clone())).await;
        assert_eq!(s, nf, "{who} POST {uri} {form}: {text}");
        assert_eq!(w.fingerprint().await, before, "{who} POST {uri} {form} changed something");
    }
    // controls: a household of the same group may use the shared recipe
    let (s, _, _) = send(&w.app, "POST", "/plan/entries", Some(&w.bob), Req::Form(format!("date=2026-12-01&slot=dinner&recipe_id={}&scale=1", i.recipe))).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn lists_pages_and_parsing_show_only_the_callers_tenant() {
    let w = world().await;
    for (token, recipes, lists, plan) in [(&w.alice, 1, 1, 1), (&w.bob, 1, 0, 0), (&w.carol, 0, 0, 0)] {
        let (_, v) = json_of(&w.app, "GET", "/api/recipes?include_archived=true", token, Req::None).await;
        assert_eq!(v.as_array().unwrap().len(), recipes, "recipes {v}");
        let (_, v) = json_of(&w.app, "GET", "/api/shopping-lists", token, Req::None).await;
        assert_eq!(v.as_array().unwrap().len(), lists, "lists {v}");
        let (_, v) = json_of(&w.app, "GET", "/api/meal-plan?from=2026-01-01&to=2026-12-31", token, Req::None).await;
        assert_eq!(v.as_array().unwrap().len(), plan, "plan {v}");
    }

    let parse = |token: &'static str| {
        let (app, t) = (w.app.clone(), if token == "alice" { w.alice.clone() } else { w.carol.clone() });
        async move { json_of(&app, "POST", "/api/parse", &t, Req::Json(json!({"lines": ["250 g flour"]}))).await.1 }
    };
    let mine = parse("alice").await;
    assert_eq!((mine[0]["unit_id"].as_i64(), mine[0]["food_id"].as_i64()), (Some(w.ids.unit), Some(w.ids.food)), "{mine}");
    let theirs = parse("carol").await;
    assert_eq!((theirs[0]["unit_id"].as_i64(), theirs[0]["food_id"].as_i64()), (None, None), "another group's vocabulary is not matched: {theirs}");

    // HTML pages
    let page = |token: &str, uri: &str| {
        let (app, t, uri) = (w.app.clone(), token.to_string(), uri.to_string());
        async move { send(&app, "GET", &uri, Some(&t), Req::None).await.2 }
    };
    assert!(page(&w.alice, "/").await.contains("Pancakes"));
    assert!(page(&w.bob, "/").await.contains("Pancakes"), "shared within the group");
    assert!(!page(&w.carol, "/").await.contains("Pancakes"));
    assert!(page(&w.alice, "/plan?week=2026-11-30").await.contains("private-plan-note"));
    assert!(!page(&w.bob, "/plan?week=2026-11-30").await.contains("private-plan-note"));
    assert!(!page(&w.carol, "/plan?week=2026-11-30").await.contains("private-plan-note"));
    assert!(page(&w.alice, "/lists").await.contains("Weekly-secret"));
    assert!(!page(&w.bob, "/lists").await.contains("Weekly-secret"));
    assert!(!page(&w.carol, "/lists").await.contains("Weekly-secret"));
    let batch_link = format!("/batches/{}", w.ids.batch);
    assert!(page(&w.alice, &format!("/recipes/{}", w.ids.recipe)).await.contains(&batch_link));
    assert!(!page(&w.bob, &format!("/recipes/{}", w.ids.recipe)).await.contains(&batch_link), "the recipe page lists only the household's own batches");
}

// ---------- images ----------

#[tokio::test]
async fn images_are_served_only_to_the_tenant_that_owns_them() {
    let w = world().await;
    let recipe_img = format!("{}.png", "a".repeat(64));
    let loose = format!("{}.png", "c".repeat(64));
    std::fs::write(w.images.join(&recipe_img), b"recipe image").unwrap();
    std::fs::write(w.images.join(&loose), b"nobody's image").unwrap();
    sqlx::query("INSERT INTO images (hash, mime) VALUES (?, 'image/png')").bind("a".repeat(64)).execute(w.db.pool()).await.unwrap();
    sqlx::query("INSERT INTO recipe_images (recipe_id, hash) VALUES (?, ?)").bind(w.ids.recipe).bind("a".repeat(64)).execute(w.db.pool()).await.unwrap();
    let photo = w.db.add_batch_photo(&w.t.a, w.ids.batch, &w.images, &common::png(4, 4, 9)).await.unwrap().file_name();

    let status = |token: &str, name: &str| {
        let (app, t, uri) = (w.app.clone(), token.to_string(), format!("/images/{name}"));
        async move { send(&app, "GET", &uri, Some(&t), Req::None).await.0 }
    };
    // a recipe's image belongs to its group
    assert_eq!(status(&w.alice, &recipe_img).await, StatusCode::OK);
    assert_eq!(status(&w.bob, &recipe_img).await, StatusCode::OK);
    assert_eq!(status(&w.carol, &recipe_img).await, StatusCode::NOT_FOUND);
    // a batch photo to its household
    assert_eq!(status(&w.alice, &photo).await, StatusCode::OK);
    assert_eq!(status(&w.bob, &photo).await, StatusCode::NOT_FOUND);
    assert_eq!(status(&w.carol, &photo).await, StatusCode::NOT_FOUND);
    // a file nothing refers to is nobody's
    assert_eq!(status(&w.alice, &loose).await, StatusCode::NOT_FOUND);
    // and the batch page of another household does not name it
    let (s, _, _) = send(&w.app, "GET", &format!("/batches/{}", w.ids.batch), Some(&w.bob), Req::None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

// ---------- S37: admin household management over HTTP ----------

struct Admin {
    app: Router,
    db: Db,
    admin: String,
    user: String,
    user_id: i64,
    home: i64,
}

async fn admin_world() -> Admin {
    let db = Db::open_memory().await.unwrap();
    let home = db.default_household().await.unwrap();
    let (_, admin) = user_in(&db, "root", "admin", home.id).await;
    let (user_id, user) = user_in(&db, "plain", "user", home.id).await;
    let app = api::router(api::AppState { db: db.clone(), images_dir: tempfile::tempdir().unwrap().keep() });
    Admin { app, db, admin, user, user_id, home: home.id }
}

#[tokio::test]
async fn household_management_is_admin_only_on_every_route() {
    let a = admin_world().await;
    let routes: [(&str, &str, Req); 12] = [
        ("GET", "/api/groups", Req::None),
        ("POST", "/api/groups", Req::Json(json!({"name": "X"}))),
        ("GET", "/api/households", Req::None),
        ("POST", "/api/households", Req::Json(json!({"group_id": 1, "name": "X"}))),
        ("GET", "/api/default-household", Req::None),
        ("PUT", "/api/default-household", Req::Json(json!({"household_id": 1}))),
        ("PUT", "/api/users/1/household", Req::Json(json!({"household_id": 1}))),
        ("GET", "/settings/households", Req::None),
        ("POST", "/settings/households/groups", Req::Form("name=X".into())),
        ("POST", "/settings/households/households", Req::Form("group_id=1&name=X".into())),
        ("POST", "/settings/households/default", Req::Form("household_id=1".into())),
        ("POST", "/settings/households/move-user", Req::Form("user_id=1&household_id=1".into())),
    ];
    for (m, uri, body) in routes {
        let form = matches!(body, Req::Form(_));
        let again = match &body {
            Req::Json(v) => Req::Json(v.clone()),
            Req::Form(f) => Req::Form(f.clone()),
            _ => Req::None,
        };
        let (s, ..) = send(&a.app, m, uri, Some(&a.user), body).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{m} {uri} as a plain user");
        let (s, h, _) = send(&a.app, m, uri, None, again).await;
        assert!(matches!(s, StatusCode::UNAUTHORIZED | StatusCode::SEE_OTHER), "{m} {uri} anonymous: {s} {h:?} (form: {form})");
    }
    assert_eq!(a.db.groups().await.unwrap().len(), 1, "nothing was created by the refused requests");
    // the guard refuses the whole area, not only the routes that exist: a route added later without `Admin` is still covered
    for uri in ["/api/groups/x", "/api/households/x", "/api/default-household/x", "/settings/households/x"] {
        let (s, ..) = send(&a.app, "GET", uri, Some(&a.user), Req::None).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{uri}");
    }
}

#[tokio::test]
async fn an_admin_creates_groups_and_households_and_chooses_the_default_over_the_api() {
    let a = admin_world().await;
    let (s, g) = json_of(&a.app, "POST", "/api/groups", &a.admin, Req::Json(json!({"name": "Family Kitchen"}))).await;
    assert_eq!((s, g["name"].as_str(), g["slug"].as_str()), (StatusCode::CREATED, Some("Family Kitchen"), Some("family-kitchen")), "{g}");
    let gid = g["id"].as_i64().unwrap();
    let (s, h) = json_of(&a.app, "POST", "/api/households", &a.admin, Req::Json(json!({"group_id": gid, "name": "Upstairs"}))).await;
    assert_eq!((s, h["group_id"].as_i64(), h["slug"].as_str()), (StatusCode::CREATED, Some(gid), Some("upstairs")), "{h}");
    let hid = h["id"].as_i64().unwrap();

    let (_, groups) = json_of(&a.app, "GET", "/api/groups", &a.admin, Req::None).await;
    assert_eq!(groups.as_array().unwrap().iter().map(|g| g["slug"].as_str().unwrap()).collect::<Vec<_>>(), vec!["home", "family-kitchen"]);
    let (_, hs) = json_of(&a.app, "GET", "/api/households", &a.admin, Req::None).await;
    assert_eq!(hs.as_array().unwrap().len(), 2);

    let (s, d) = json_of(&a.app, "GET", "/api/default-household", &a.admin, Req::None).await;
    assert_eq!((s, d["id"].as_i64()), (StatusCode::OK, Some(a.home)), "{d}");
    let (s, d) = json_of(&a.app, "PUT", "/api/default-household", &a.admin, Req::Json(json!({"household_id": hid}))).await;
    assert_eq!((s, d["id"].as_i64()), (StatusCode::OK, Some(hid)), "{d}");
    assert_eq!(a.db.default_household().await.unwrap().id, hid);

    // errors
    let (s, v) = json_of(&a.app, "POST", "/api/groups", &a.admin, Req::Json(json!({"name": "  "}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let (s, _) = json_of(&a.app, "POST", "/api/households", &a.admin, Req::Json(json!({"group_id": 999, "name": "X"}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = json_of(&a.app, "PUT", "/api/default-household", &a.admin, Req::Json(json!({"household_id": 999}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(a.db.default_household().await.unwrap().id, hid, "a refused change left the default alone");
}

#[tokio::test]
async fn moving_a_user_over_the_api_changes_what_their_next_request_sees() {
    let a = admin_world().await;
    let t = common::tenants(&a.db).await;
    a.db.create_recipe(&t.c, RecipeInput { name: "Gamma soup".into(), ..Default::default() }).await.unwrap();
    let (_, before) = json_of(&a.app, "GET", "/api/recipes", &a.user, Req::None).await;
    assert!(before.as_array().unwrap().is_empty());
    let (_, me) = json_of(&a.app, "GET", "/api/me", &a.user, Req::None).await;
    assert_eq!(me["household_id"].as_i64(), Some(a.home), "{me}");

    let uri = format!("/api/users/{}/household", a.user_id);
    let (s, u) = json_of(&a.app, "PUT", &uri, &a.admin, Req::Json(json!({"household_id": t.c.household_id}))).await;
    assert_eq!((s, u["household_id"].as_i64(), u["group_id"].as_i64()), (StatusCode::OK, Some(t.c.household_id), Some(t.c.group_id)), "{u}");
    let (_, after) = json_of(&a.app, "GET", "/api/recipes", &a.user, Req::None).await;
    assert_eq!(after.as_array().unwrap().len(), 1, "same token, new tenant: {after}");

    let (s, _) = json_of(&a.app, "PUT", &uri, &a.admin, Req::Json(json!({"household_id": 999}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = json_of(&a.app, "PUT", "/api/users/999/household", &a.admin, Req::Json(json!({"household_id": t.c.household_id}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = json_of(&a.app, "PUT", &uri, &a.user, Req::Json(json!({"household_id": a.home}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "a user cannot move themselves back");
}

#[tokio::test]
async fn the_households_page_lists_and_changes_things() {
    let a = admin_world().await;
    let t = common::tenants(&a.db).await;
    let (s, _, page) = send(&a.app, "GET", "/settings/households", Some(&a.admin), Req::None).await;
    assert_eq!(s, StatusCode::OK);
    for want in ["Home", "One", "Two", "Alpha", "Beta", "Gamma", "root", "plain", "/settings/households/groups", "/settings/households/households", "/settings/households/default", "/settings/households/move-user"] {
        assert!(page.contains(want), "page lacks {want:?}");
    }

    let post = |uri: &'static str, form: String| {
        let (app, t) = (a.app.clone(), a.admin.clone());
        async move { send(&app, "POST", uri, Some(&t), Req::Form(form)).await }
    };
    let (s, h, _) = post("/settings/households/groups", "name=Cousins".into()).await;
    assert_eq!((s, h["location"].to_str().unwrap()), (StatusCode::SEE_OTHER, "/settings/households"));
    let cousins = a.db.groups().await.unwrap().into_iter().find(|g| g.name == "Cousins").unwrap();
    let (s, ..) = post("/settings/households/households", format!("group_id={}&name=Basement", cousins.id)).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    let basement = a.db.households().await.unwrap().into_iter().find(|h| h.name == "Basement").unwrap();
    assert_eq!(basement.group_id, cousins.id);
    let (s, ..) = post("/settings/households/default", format!("household_id={}", basement.id)).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(a.db.default_household().await.unwrap().id, basement.id);
    let (s, ..) = post("/settings/households/move-user", format!("user_id={}&household_id={}", a.user_id, t.b.household_id)).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(a.db.user(a.user_id).await.unwrap().household_id, t.b.household_id);
    let (_, _, page) = send(&a.app, "GET", "/settings/households", Some(&a.admin), Req::None).await;
    assert!(page.contains("Basement") && page.contains("Cousins"));

    // refusals are the usual error responses, and change nothing
    let (s, ..) = post("/settings/households/groups", "name=".into()).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, ..) = post("/settings/households/households", "group_id=999&name=X".into()).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, ..) = post("/settings/households/default", "household_id=999".into()).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, ..) = post("/settings/households/move-user", format!("user_id={}&household_id=999", a.user_id)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(a.db.user(a.user_id).await.unwrap().household_id, t.b.household_id);
}

#[tokio::test]
async fn the_navigation_links_to_the_households_page() {
    let a = admin_world().await;
    let (_, _, page) = send(&a.app, "GET", "/", Some(&a.admin), Req::None).await;
    assert!(page.contains("href=\"/settings/households\""), "{page}");
}

#[tokio::test]
async fn local_admin_login_lands_in_the_default_household() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    db.set_default_household(t.c.household_id).await.unwrap();
    let cfg = AuthConfig { admin_hash: Some(hash_password("correct horse battery").unwrap()), ..AuthConfig::default() };
    let app = api::router_with_auth(api::AppState { db: db.clone(), images_dir: tempfile::tempdir().unwrap().keep() }, ImportService::default(), cfg);
    let form = serde_urlencoded::to_string([("username", "admin"), ("password", "correct horse battery")]).unwrap();
    let req = Request::builder().method("POST").uri("/login/local").header("content-type", "application/x-www-form-urlencoded");
    let resp = app.clone().oneshot(req.body(Body::from(form)).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let cookie = resp.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_string();
    let me = Request::builder().uri("/api/me").header("cookie", cookie).body(Body::empty()).unwrap();
    let resp = app.clone().oneshot(me).await.unwrap();
    let v: Value = serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!((v["household_id"].as_i64(), v["group_id"].as_i64(), v["role"].as_str()), (Some(t.c.household_id), Some(t.c.group_id), Some("admin")), "{v}");
}
