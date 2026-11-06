//! Step 14, part B through the router: favourites and ratings, recipe actions, the pantry, and
//! skipping foods on hand when a recipe goes on a list.
mod common;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use mealier::{ActionInput, Db, Dimension, IngredientInput, RecipeInput, Scope, api};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(app: &Router, method: &str, uri: &str, bearer: &str, body: Option<(&str, String)>) -> (StatusCode, String) {
    let mut req = Request::builder().method(method).uri(uri).header("authorization", bearer);
    let body = match body {
        Some((ct, b)) => {
            req = req.header("content-type", ct);
            Body::from(b)
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    (resp.status(), String::from_utf8_lossy(&resp.into_body().collect().await.unwrap().to_bytes()).into_owned())
}

fn json_body(v: Value) -> Option<(&'static str, String)> {
    Some(("application/json", v.to_string()))
}

fn form(f: &str) -> Option<(&'static str, String)> {
    Some(("application/x-www-form-urlencoded", f.to_string()))
}

async fn user_in(db: &Db, name: &str, role: &str, household_id: i64) -> String {
    let id: i64 = sqlx::query_scalar("INSERT INTO users (username, email, role, household_id) VALUES (?, ?, ?, ?) RETURNING id").bind(name).bind(format!("{name}@x")).bind(role).bind(household_id).fetch_one(db.pool()).await.unwrap();
    let secret = format!("mlr_{name}-token");
    sqlx::query("INSERT INTO api_tokens (hash, user_id, name) VALUES (?, ?, 't')").bind(mealier::hash_token(&secret)).bind(id).execute(db.pool()).await.unwrap();
    format!("Bearer {secret}")
}

struct World {
    app: Router,
    db: Db,
    t: common::Tenants,
    ann: String,
    /// Same household as ann.
    bo: String,
    admin: String,
    recipe: i64,
    soup: i64,
}

async fn world() -> World {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let ann = user_in(&db, "ann", "user", t.a.household_id).await;
    let bo = user_in(&db, "bo", "user", t.a.household_id).await;
    let admin = user_in(&db, "root", "admin", t.a.household_id).await;
    let g = db.add_unit(&t.a, "g", Dimension::Mass, Some(1.0)).await.unwrap();
    let (flour, salt) = (db.add_food(&t.a, "flour").await.unwrap(), db.add_food(&t.a, "salt").await.unwrap());
    let line = |f| IngredientInput { qty_min: Some(1.0), qty_max: Some(1.0), unit_id: Some(g), food_id: Some(f), ..Default::default() };
    let recipe = db.create_recipe(&t.a, RecipeInput { name: "Pancakes".into(), ingredients: vec![line(flour), line(salt)], tools: vec!["whisk".into()], ..Default::default() }).await.unwrap().id;
    let soup = db.create_recipe(&t.a, RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap().id;
    let app = api::router(api::AppState { db: db.clone(), images_dir: tempfile::tempdir().unwrap().keep() });
    World { app, db, t, ann, bo, admin, recipe, soup }
}

fn admin(sc: Scope) -> Scope {
    Scope { admin: true, ..sc }
}

// ---------- favourites and ratings ----------

#[tokio::test]
async fn the_api_sets_and_reads_my_own_favourite_and_rating_and_lists_only_my_favourites() {
    let w = world().await;
    let uri = |p: &str| format!("/api/recipes/{}/{p}", w.recipe);
    let (s, _) = call(&w.app, "PUT", &uri("favourite"), &w.ann, json_body(json!({"favourite": true}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = call(&w.app, "PUT", &uri("rating"), &w.ann, json_body(json!({"rating": 4.5}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, mine) = call(&w.app, "GET", &uri("mine"), &w.ann, None).await;
    assert_eq!(serde_json::from_str::<Value>(&mine).unwrap(), json!({"favourite": true, "rating": 4.5}));
    let (_, theirs) = call(&w.app, "GET", &uri("mine"), &w.bo, None).await;
    assert_eq!(serde_json::from_str::<Value>(&theirs).unwrap(), json!({"favourite": false, "rating": null}), "same household, different user");

    let names = |body: String| serde_json::from_str::<Value>(&body).unwrap().as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(names(call(&w.app, "GET", "/api/recipes?favourites=true", &w.ann, None).await.1), ["Pancakes"]);
    assert!(names(call(&w.app, "GET", "/api/recipes?favourites=true", &w.bo, None).await.1).is_empty());
    assert_eq!(names(call(&w.app, "GET", "/api/recipes", &w.bo, None).await.1), ["Pancakes", "Soup"]);

    for bad in [json!({"rating": 3.7}), json!({"rating": 0}), json!({"rating": 6})] {
        let (s, _) = call(&w.app, "PUT", &uri("rating"), &w.ann, json_body(bad.clone())).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
    let (s, _) = call(&w.app, "PUT", &uri("rating"), &w.ann, json_body(json!({"rating": null}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, mine) = call(&w.app, "GET", &uri("mine"), &w.ann, None).await;
    assert_eq!(serde_json::from_str::<Value>(&mine).unwrap()["rating"], Value::Null);
}

#[tokio::test]
async fn the_recipe_page_has_a_star_toggle_and_a_rating_select_that_show_my_state() {
    let w = world().await;
    let page = |who: &str| {
        let (app, uri, who) = (w.app.clone(), format!("/recipes/{}", w.recipe), who.to_string());
        async move { call(&app, "GET", &uri, &who, None).await.1 }
    };
    let p = page(&w.ann).await;
    assert!(p.contains(&format!("action=\"/recipes/{}/favourite\"", w.recipe)) && p.contains("name=\"favourite\" value=\"1\""), "{p}");
    assert!(p.contains("name=\"rating\"") && p.contains("<option value=\"4.5\">") && !p.contains(" selected>4.5"), "{p}");

    let (s, _) = call(&w.app, "POST", &format!("/recipes/{}/favourite", w.recipe), &w.ann, form("favourite=1")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    let (s, _) = call(&w.app, "POST", &format!("/recipes/{}/rating", w.recipe), &w.ann, form("rating=4.5")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    let p = page(&w.ann).await;
    assert!(p.contains("name=\"favourite\" value=\"0\""), "the toggle now unfavourites: {p}");
    assert!(p.contains("<option value=\"4.5\" selected>4.5"), "{p}");
    let q = page(&w.bo).await;
    assert!(q.contains("name=\"favourite\" value=\"1\"") && !q.contains(" selected>4.5"), "another user's page is unmarked");

    let (s, _) = call(&w.app, "POST", &format!("/recipes/{}/rating", w.recipe), &w.ann, form("rating=")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert!(!page(&w.ann).await.contains(" selected>4.5"), "a blank rating clears it");
    let (s, _) = call(&w.app, "POST", &format!("/recipes/{}/rating", w.recipe), &w.ann, form("rating=3.7")).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn the_recipe_list_can_be_limited_to_my_favourites() {
    let w = world().await;
    call(&w.app, "POST", &format!("/recipes/{}/favourite", w.soup), &w.ann, form("favourite=1")).await;
    let (_, all) = call(&w.app, "GET", "/", &w.ann, None).await;
    assert!(all.contains("Pancakes") && all.contains("Soup") && all.contains("href=\"/?favourites=1\""), "{all}");
    let (_, fav) = call(&w.app, "GET", "/?favourites=1", &w.ann, None).await;
    assert!(fav.contains("Soup") && !fav.contains(">Pancakes<"), "{fav}");
    let (_, other) = call(&w.app, "GET", "/?favourites=1", &w.bo, None).await;
    assert!(!other.contains(">Soup<") && !other.contains(">Pancakes<") && other.contains("No favourites yet"), "{other}");
}

// ---------- actions ----------

#[tokio::test]
async fn a_link_action_is_an_anchor_with_a_safe_rel_and_only_when_it_comes_out_as_http() {
    let w = world().await;
    w.db.create_action(&w.t.a, ActionInput { title: "Search".into(), kind: "link".into(), url: "https://www.google.com/search?q=${slug}&n=${id}".into() }).await.unwrap();
    // one that got into the table some other way (an import): it must not become a link
    sqlx::query("INSERT INTO recipe_actions (household_id, title, kind, url) VALUES (?, 'Evil', 'link', 'javascript:alert(${id})')").bind(w.t.a.household_id).execute(w.db.pool()).await.unwrap();
    let (_, page) = call(&w.app, "GET", &format!("/recipes/{}", w.recipe), &w.ann, None).await;
    let anchor = format!("<a href=\"https://www.google.com/search?q=pancakes&#38;n={}\" rel=\"noopener noreferrer\"", w.recipe);
    assert!(page.contains(&anchor), "{page}");
    assert!(!page.contains("Evil") && !page.contains("javascript:"), "{page}");
    // another household sees none
    let other = user_in(&w.db, "dee", "user", w.t.b.household_id).await;
    let (_, page) = call(&w.app, "GET", &format!("/recipes/{}", w.recipe), &other, None).await;
    assert!(!page.contains("google.com"));
}

#[tokio::test]
async fn only_an_admin_creates_or_edits_a_post_action_over_the_api_and_the_form() {
    let w = world().await;
    let post = json!({"title": "Hook", "kind": "post", "url": "http://hass.lan/api/webhook/x"});
    let (s, _) = call(&w.app, "POST", "/api/recipe-actions", &w.ann, json_body(post.clone())).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call(&w.app, "POST", "/settings/actions", &w.ann, form("title=Hook&kind=post&url=http%3A%2F%2Fhass.lan%2Fx")).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, link) = call(&w.app, "POST", "/api/recipe-actions", &w.ann, json_body(json!({"title": "Look", "kind": "link", "url": "https://x.test/${id}"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{link}");
    let (s, hook) = call(&w.app, "POST", "/api/recipe-actions", &w.admin, json_body(post)).await;
    assert_eq!(s, StatusCode::CREATED, "{hook}");
    let hook_id = serde_json::from_str::<Value>(&hook).unwrap()["id"].as_i64().unwrap();
    let (s, _) = call(&w.app, "PUT", &format!("/api/recipe-actions/{hook_id}"), &w.ann, json_body(json!({"title": "x", "kind": "post", "url": "http://evil/"}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // a household member sees and may run it, but the settings page offers them no post kind
    let (_, mine) = call(&w.app, "GET", "/settings/actions", &w.ann, None).await;
    assert!(mine.contains("Hook") && !mine.contains("<option value=\"post\">"), "{mine}");
    let (_, theirs) = call(&w.app, "GET", "/settings/actions", &w.admin, None).await;
    assert!(theirs.contains("<option value=\"post\">"));
    let (s, ..) = call(&w.app, "POST", "/settings/actions", &w.admin, form("title=Second&kind=link&url=https%3A%2F%2Fx.test%2F")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn a_post_action_button_sends_the_recipe_and_the_api_reports_the_status() {
    let w = world().await;
    use std::sync::{Arc, Mutex};
    let got: Arc<Mutex<Vec<String>>> = Default::default();
    let g = got.clone();
    let rx = axum::Router::new().route("/hook", axum::routing::post(move |body: String| async move { g.lock().unwrap().push(body); axum::http::StatusCode::ACCEPTED }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, rx).await.unwrap() });
    let a = w.db.create_action(&admin(w.t.a), ActionInput { title: "Hook".into(), kind: "post".into(), url }).await.unwrap();

    let (_, page) = call(&w.app, "GET", &format!("/recipes/{}", w.recipe), &w.ann, None).await;
    assert!(page.contains(&format!("action=\"/recipes/{}/actions/{}/run\"", w.recipe, a.id)) && page.contains("Hook"), "{page}");
    let (s, body) = call(&w.app, "POST", &format!("/api/recipe-actions/{}/trigger", a.id), &w.ann, json_body(json!({"recipe_id": w.recipe}))).await;
    assert_eq!((s, serde_json::from_str::<Value>(&body).unwrap()["status"].as_u64()), (StatusCode::OK, Some(202)), "{body}");
    let (s, _) = call(&w.app, "POST", &format!("/recipes/{}/actions/{}/run", w.recipe, a.id), &w.ann, form("")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    let sent = got.lock().unwrap().clone();
    assert_eq!(sent.len(), 2);
    assert_eq!(serde_json::from_str::<Value>(&sent[0]).unwrap()["name"], "Pancakes");
}

// ---------- pantry ----------

#[tokio::test]
async fn the_pantry_page_and_api_toggle_foods_and_tools_for_my_household_only() {
    let w = world().await;
    let (_, p) = call(&w.app, "GET", "/api/pantry", &w.ann, None).await;
    let p: Value = serde_json::from_str(&p).unwrap();
    let salt = p["foods"].as_array().unwrap().iter().find(|f| f["name"] == "salt").unwrap()["id"].as_i64().unwrap();
    let whisk = p["tools"][0]["id"].as_i64().unwrap();
    let (s, _) = call(&w.app, "PUT", &format!("/api/foods/{salt}/on-hand"), &w.ann, json_body(json!({"on_hand": true}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = call(&w.app, "POST", &format!("/pantry/tools/{whisk}"), &w.ann, form("have=1")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);

    // bo shares the household and sees it; dee, in another household of the group, does not
    let dee = user_in(&w.db, "dee", "user", w.t.b.household_id).await;
    let (_, page) = call(&w.app, "GET", "/pantry", &w.bo, None).await;
    assert!(page.contains("On hand: remove") && page.contains("Owned: remove"), "{page}");
    let (_, page) = call(&w.app, "GET", "/pantry", &dee, None).await;
    assert!(page.contains("Mark on hand") && page.contains("Mark owned") && !page.contains("On hand: remove") && !page.contains("Owned: remove"), "{page}");
    let (s, _) = call(&w.app, "POST", &format!("/pantry/foods/{salt}"), &w.ann, form("have=0")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    let (_, page) = call(&w.app, "GET", "/pantry", &w.bo, None).await;
    assert!(!page.contains("On hand: remove"));
    let (s, _) = call(&w.app, "POST", &format!("/pantry/foods/{salt}"), &w.ann, form("have=1")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    let (_, page) = call(&w.app, "GET", "/pantry", &w.bo, None).await;
    assert!(page.contains("On hand: remove"), "the form can mark a food too");
}

#[tokio::test]
async fn a_recipe_goes_on_a_list_without_the_foods_on_hand_when_asked_over_the_api_and_the_form() {
    let w = world().await;
    let flour = w.db.pantry(&w.t.a).await.unwrap().foods.iter().find(|f| f.name == "flour").unwrap().id;
    w.db.set_food_on_hand(&w.t.a, flour, true).await.unwrap();
    let list = |name: &'static str| {
        let (db, sc) = (w.db.clone(), w.t.a);
        async move { db.create_list(&sc, name).await.unwrap().id }
    };
    let (all, skip, ticked, unticked) = (list("all").await, list("skip").await, list("ticked").await, list("unticked").await);
    let count = |id: i64| {
        let (db, sc) = (w.db.clone(), w.t.a);
        async move { db.shopping_list(&sc, id).await.unwrap().items.len() }
    };
    call(&w.app, "POST", &format!("/api/shopping-lists/{all}/recipes"), &w.ann, json_body(json!({"recipe_id": w.recipe, "scale": 1}))).await;
    call(&w.app, "POST", &format!("/api/shopping-lists/{skip}/recipes"), &w.ann, json_body(json!({"recipe_id": w.recipe, "scale": 1, "skip_on_hand": true}))).await;
    call(&w.app, "POST", "/lists/add-recipe", &w.ann, form(&format!("list_id={ticked}&recipe_id={}&scale=1&skip_on_hand=1", w.recipe))).await;
    call(&w.app, "POST", "/lists/add-recipe", &w.ann, form(&format!("list_id={unticked}&recipe_id={}&scale=1", w.recipe))).await;
    assert_eq!((count(all).await, count(skip).await, count(ticked).await, count(unticked).await), (2, 1, 1, 2));
    let (_, page) = call(&w.app, "GET", &format!("/recipes/{}", w.recipe), &w.ann, None).await;
    assert!(page.contains("name=\"skip_on_hand\"") && !page.contains("name=\"skip_on_hand\" checked"), "off by default: {page}");
}
