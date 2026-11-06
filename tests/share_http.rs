//! Step 14, part B: the public share pages and their management routes, through the router.
mod common;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use mealier::{Db, Dimension, IngredientInput, Note, Nutrition, RecipeInput, Settings, api};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn send(app: &Router, method: &str, uri: &str, bearer: Option<&str>, form: Option<&str>) -> (StatusCode, HeaderMap, String) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(b) = bearer {
        req = req.header("authorization", b);
    }
    let body = match form {
        Some(f) => {
            req = req.header("content-type", "application/x-www-form-urlencoded");
            Body::from(f.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let (status, h) = (resp.status(), resp.headers().clone());
    (status, h, String::from_utf8_lossy(&resp.into_body().collect().await.unwrap().to_bytes()).into_owned())
}

async fn user_in(db: &Db, name: &str, household_id: i64) -> String {
    let id: i64 = sqlx::query_scalar("INSERT INTO users (username, email, role, household_id) VALUES (?, ?, 'user', ?) RETURNING id").bind(name).bind(format!("{name}@x")).bind(household_id).fetch_one(db.pool()).await.unwrap();
    let secret = format!("mlr_{name}-token");
    sqlx::query("INSERT INTO api_tokens (hash, user_id, name) VALUES (?, ?, 't')").bind(mealier::hash_token(&secret)).bind(id).execute(db.pool()).await.unwrap();
    format!("Bearer {secret}")
}

struct World {
    app: Router,
    db: Db,
    t: common::Tenants,
    images: std::path::PathBuf,
    alice: String,
    carol: String,
    recipe: i64,
    other: i64,
    image: String,
    asset: String,
    /// An asset of the other group's recipe.
    other_asset: String,
}

async fn world() -> World {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let alice = user_in(&db, "alice", t.a.household_id).await;
    let carol = user_in(&db, "carol", t.c.household_id).await;
    let g = db.add_unit(&t.a, "g", Dimension::Mass, Some(1.0)).await.unwrap();
    let flour = db.add_food(&t.a, "flour").await.unwrap();
    let recipe = db
        .create_recipe(
            &t.a,
            RecipeInput {
                name: "Pancakes".into(),
                description: "Fluffy things".into(),
                ingredients: vec![IngredientInput { qty_min: Some(250.0), qty_max: Some(250.0), unit_id: Some(g), food_id: Some(flour), ..Default::default() }],
                steps: vec!["Whisk it all".into()],
                notes: Some(vec![Note { title: "Tip".into(), text: "Rest the batter".into() }]),
                nutrition: Some(Nutrition { calories: Some(321.0), ..Default::default() }),
                tags: vec!["secret-tag".into()],
                source_url: Some("https://source.example/pancakes".into()),
                extras: Some([("secret-extra".to_string(), "x".to_string())].into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .id;
    let other = db.create_recipe(&t.c, RecipeInput { name: "Secret Cassoulet".into(), ..Default::default() }).await.unwrap().id;
    // household and user data that must never show
    let b = db.start_batch(&t.a, recipe, 1.0).await.unwrap();
    db.update_batch(&t.a, b.batch.id, mealier::BatchInput { scale: 1.0, notes: "private-batch-note".into(), lines: b.lines.iter().map(|l| mealier::BatchLineInput { id: Some(l.id), qty_min: l.qty_min, qty_max: l.qty_max, unit_id: l.unit_id, ..Default::default() }).collect(), ..Default::default() }).await.unwrap();
    db.add_entry(&t.a, mealier::EntryInput { date: "2026-12-01".into(), slot: "dinner".into(), recipe_id: Some(recipe), text: "private-plan-note".into(), ..Default::default() }).await.unwrap();
    db.create_list(&t.a, "Weekly-secret").await.unwrap();
    db.add_comment(&t.a, recipe, "private-comment").await.unwrap();

    let images = tempfile::tempdir().unwrap().keep();
    let image = format!("{}.png", "a".repeat(64));
    std::fs::write(images.join(&image), b"png bytes").unwrap();
    sqlx::query("INSERT INTO images (hash, mime) VALUES (?, 'image/png')").bind("a".repeat(64)).execute(db.pool()).await.unwrap();
    sqlx::query("INSERT INTO recipe_images (recipe_id, hash) VALUES (?, ?)").bind(recipe).bind("a".repeat(64)).execute(db.pool()).await.unwrap();
    let asset = db.add_recipe_asset(&t.a, recipe, &images.join("assets"), "menu.txt", "Menu", "", b"asset body").await.unwrap().hash;
    let other_asset = db.add_recipe_asset(&t.c, other, &images.join("assets"), "theirs.txt", "Theirs", "", b"their asset").await.unwrap().hash;
    let app = api::router(api::AppState { db: db.clone(), images_dir: images.clone() });
    World { app, db, t, images, alice, carol, recipe, other, image, asset, other_asset }
}

impl World {
    async fn token(&self) -> String {
        self.db.create_share_link(&self.t.a, self.recipe, None).await.unwrap().token
    }
    async fn get(&self, uri: &str) -> (StatusCode, HeaderMap, String) {
        send(&self.app, "GET", uri, None, None).await
    }
}

#[tokio::test]
async fn the_page_shows_the_recipe_to_anyone_without_a_login() {
    let w = world().await;
    let token = w.token().await;
    let (s, h, page) = w.get(&format!("/share/{token}")).await;
    assert_eq!(s, StatusCode::OK);
    for want in ["Pancakes", "Fluffy things", "250 g flour", "Whisk it all", "Tip", "Rest the batter", "Calories: 321"] {
        assert!(page.contains(want), "page lacks {want:?}: {page}");
    }
    assert_eq!(h["referrer-policy"], "no-referrer");
    assert_eq!(h["cache-control"], "private, no-store");
    assert!(h["content-type"].to_str().unwrap().starts_with("text/html"));
}

#[tokio::test]
async fn the_page_shows_nothing_but_the_recipe() {
    let w = world().await;
    let token = w.token().await;
    let (s, _, page) = w.get(&format!("/share/{token}")).await;
    assert!(s == StatusCode::OK && page.contains("Pancakes"), "{s}: {page}");
    for leak in [
        "private-batch-note", "private-plan-note", "private-comment", "Weekly-secret", "Secret Cassoulet", "secret-tag", "secret-extra", "source.example",
        "href=\"/", "<form", "/recipes/", "/batches/", "/settings", "/plan", "/lists", "/cookbooks", "Sign out", "<nav", "/images/",
    ] {
        // the page's own links all live under /share/<token>/
        let page = page.replace(&format!("/share/{token}/"), "").replace("/static/", "");
        assert!(!page.contains(leak), "the share page leaks {leak:?}: {page}");
    }
}

#[tokio::test]
async fn hidden_nutrition_stays_hidden() {
    let w = world().await;
    let token = w.token().await;
    w.db.update_recipe(&w.t.a, w.recipe, RecipeInput { name: "Pancakes".into(), nutrition: Some(Nutrition { calories: Some(321.0), ..Default::default() }), settings: Some(Settings { hide_nutrition: true, ..Default::default() }), ..Default::default() }).await.unwrap();
    let (_, _, page) = w.get(&format!("/share/{token}")).await;
    assert!(page.contains("Pancakes") && !page.contains("321") && !page.contains("Nutrition"), "{page}");
}

#[tokio::test]
async fn unknown_expired_and_revoked_links_all_give_the_same_404() {
    let w = world().await;
    let old = w.db.create_share_link(&w.t.a, w.recipe, None).await.unwrap();
    sqlx::query("UPDATE share_links SET expires_at = '2020-01-01T00:00:00Z' WHERE id = ?").bind(old.link.id).execute(w.db.pool()).await.unwrap();
    let gone = w.db.create_share_link(&w.t.a, w.recipe, None).await.unwrap();
    w.db.revoke_share_link(&w.t.a, gone.link.id).await.unwrap();
    let live = w.token().await;
    let (s, _, _) = w.get(&format!("/share/{live}")).await;
    assert_eq!(s, StatusCode::OK);

    let mut seen = Vec::new();
    for t in [old.token.as_str(), gone.token.as_str(), "unknown-token", &"x".repeat(500)] {
        let (s, h, body) = w.get(&format!("/share/{t}")).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{t}");
        assert_eq!((h["referrer-policy"].to_str().unwrap(), h["cache-control"].to_str().unwrap()), ("no-referrer", "private, no-store"));
        assert!(!body.contains(t) || t.len() < 20, "the 404 does not echo the token");
        assert!(!body.contains("href=\"/"), "no navigation on the 404 page: {body}");
        seen.push((h["content-type"].clone(), body));
    }
    assert!(seen.windows(2).all(|p| p[0] == p[1]), "the four 404s differ: {seen:?}");
}

#[tokio::test]
async fn a_token_of_one_group_reveals_nothing_of_another() {
    let w = world().await;
    let a = w.token().await;
    let c = w.db.create_share_link(&w.t.c, w.other, None).await.unwrap().token;
    let (_, _, pa) = w.get(&format!("/share/{a}")).await;
    let (_, _, pc) = w.get(&format!("/share/{c}")).await;
    assert!(pa.contains("Pancakes") && !pa.contains("Cassoulet"));
    assert!(pc.contains("Cassoulet") && !pc.contains("Pancakes") && !pc.contains("Fluffy"));
    // an asset or image of the other group, asked for through this token
    for uri in [format!("/share/{a}/assets/{}", w.other_asset), format!("/share/{c}/assets/{}", w.asset), format!("/share/{c}/images/{}", w.image)] {
        let (s, ..) = w.get(&uri).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn images_are_served_through_the_link_only_for_its_recipe() {
    let w = world().await;
    let token = w.token().await;
    let (_, _, page) = w.get(&format!("/share/{token}")).await;
    assert!(page.contains(&format!("/share/{token}/images/{}", w.image)), "{page}");
    let (s, h, body) = w.get(&format!("/share/{token}/images/{}", w.image)).await;
    assert_eq!((s, body.as_str(), h["content-type"].to_str().unwrap()), (StatusCode::OK, "png bytes", "image/png"));
    assert_eq!(h["referrer-policy"], "no-referrer");
    assert_eq!(h["cache-control"], "private, no-store");
    // an image nothing here uses, a name that is not an image name, and the plain route without a login
    std::fs::write(w.images.join(format!("{}.png", "b".repeat(64))), b"x").unwrap();
    for uri in [format!("/share/{token}/images/{}.png", "b".repeat(64)), format!("/share/{token}/images/..%2F..%2Fetc%2Fpasswd"), format!("/share/{token}/images/x.png")] {
        assert_eq!(w.get(&uri).await.0, StatusCode::NOT_FOUND, "{uri}");
    }
    assert_ne!(w.get(&format!("/images/{}", w.image)).await.0, StatusCode::OK, "/images/ still needs a login");
}

#[tokio::test]
async fn assets_are_offered_only_when_the_recipe_shows_them() {
    let w = world().await;
    let token = w.token().await;
    let uri = format!("/share/{token}/assets/{}", w.asset);
    let (_, _, page) = w.get(&format!("/share/{token}")).await;
    assert!(!page.contains(&w.asset) && !page.contains("Menu"), "assets are not listed by default");
    assert_eq!(w.get(&uri).await.0, StatusCode::NOT_FOUND);

    let set = |on: bool| {
        let (db, sc, id) = (w.db.clone(), w.t.a, w.recipe);
        async move { db.update_recipe(&sc, id, RecipeInput { name: "Pancakes".into(), settings: Some(Settings { show_assets: on, ..Default::default() }), ..Default::default() }).await.unwrap() }
    };
    set(true).await;
    let (_, _, page) = w.get(&format!("/share/{token}")).await;
    assert!(page.contains(&uri) && page.contains("Menu"), "{page}");
    let (s, h, body) = w.get(&uri).await;
    assert_eq!((s, body.as_str()), (StatusCode::OK, "asset body"));
    assert!(h["content-disposition"].to_str().unwrap().starts_with("attachment"));
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert_eq!(h["referrer-policy"], "no-referrer");
    assert_eq!(h["cache-control"], "private, no-store");
    assert_eq!(w.get(&format!("/share/{token}/assets/{}", "0".repeat(64))).await.0, StatusCode::NOT_FOUND);
    assert_eq!(w.get(&format!("/share/{token}/assets/not-a-hash")).await.0, StatusCode::NOT_FOUND);
    set(false).await;
    assert_eq!(w.get(&uri).await.0, StatusCode::NOT_FOUND, "turning it off closes it again");
}

#[tokio::test]
async fn mealies_link_shape_redirects_to_the_share_page() {
    let w = world().await;
    let token = w.token().await;
    let (s, h, _) = w.get(&format!("/g/home/shared/r/{token}")).await;
    assert!(s.is_redirection(), "{s}");
    assert_eq!(h["location"], format!("/share/{token}").as_str());
    assert_eq!((h["referrer-policy"].to_str().unwrap(), h["cache-control"].to_str().unwrap()), ("no-referrer", "private, no-store"));
    // a token with anything odd in it is not followed
    let (s, h, _) = w.get("/g/home/shared/r/a%0D%0Ab%3Fx").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(h.get("location").is_none());
}

#[tokio::test]
async fn only_exact_share_paths_skip_the_login() {
    let w = world().await;
    let token = w.token().await;
    // near misses are behind the login: a redirect to it, never a page
    for uri in ["/share", "/share/", &format!("/share/{token}/"), &format!("/share/{token}/extra"), &format!("/share/{token}/assets"), &format!("/share/{token}/assets/a/b"), &format!("/sharex/{token}"), &format!("/shared/{token}"), "/g/home/shared/r", "/g/home/shared/r/", "/g/home/shared/x/abc", "/g/shared/r/abc", &format!("/prefix/share/{token}")] {
        let (s, h, _) = w.get(uri).await;
        assert!(s == StatusCode::SEE_OTHER && h["location"] == "/login", "{uri}: {s} {h:?}");
    }
    // and the management routes need a user
    let (s, ..) = send(&w.app, "POST", &format!("/api/recipes/{}/share-links", w.recipe), None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, ..) = send(&w.app, "GET", &format!("/api/recipes/{}/share-links", w.recipe), None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, ..) = send(&w.app, "DELETE", "/api/share-links/1", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_member_creates_lists_and_revokes_links_over_the_api() {
    let w = world().await;
    let json_body = |v: Value| {
        let app = w.app.clone();
        let (uri, auth) = (format!("/api/recipes/{}/share-links", w.recipe), w.alice.clone());
        async move {
            let req = Request::builder().method("POST").uri(uri).header("authorization", auth).header("content-type", "application/json").body(Body::from(v.to_string())).unwrap();
            let resp = app.oneshot(req).await.unwrap();
            let s = resp.status();
            (s, serde_json::from_slice::<Value>(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap_or(Value::Null))
        }
    };
    let (s, v) = json_body(json!({"days": 7})).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let token = v["token"].as_str().unwrap().to_string();
    assert_eq!(v["path"], format!("/share/{token}").as_str());
    assert_eq!(w.get(v["path"].as_str().unwrap()).await.0, StatusCode::OK);
    assert_eq!(json_body(json!({"days": 366})).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json_body(json!({})).await.0, StatusCode::CREATED, "days is optional");

    let (s, _, list) = send(&w.app, "GET", &format!("/api/recipes/{}/share-links", w.recipe), Some(&w.alice), None).await;
    let list: Value = serde_json::from_str(&list).unwrap();
    assert_eq!((s, list.as_array().unwrap().len()), (StatusCode::OK, 2));
    assert!(!list.to_string().contains(&token), "the list carries no token");

    let id = v["id"].as_i64().unwrap();
    let (s, ..) = send(&w.app, "DELETE", &format!("/api/share-links/{id}"), Some(&w.alice), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(w.get(&format!("/share/{token}")).await.0, StatusCode::NOT_FOUND, "revoked");
    let (s, ..) = send(&w.app, "DELETE", &format!("/api/share-links/{id}"), Some(&w.alice), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // another group can neither make nor revoke
    let (s, ..) = send(&w.app, "POST", &format!("/api/recipes/{}/share-links", w.recipe), Some(&w.carol), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_recipe_page_creates_a_link_shows_it_once_and_revokes_it() {
    let w = world().await;
    let (s, _, page) = send(&w.app, "POST", &format!("/recipes/{}/share-links", w.recipe), Some(&w.alice), Some("days=3")).await;
    assert_eq!(s, StatusCode::OK, "{page}");
    let token = page.split("/share/").nth(1).unwrap().split(['"', '<', ' ']).next().unwrap().to_string();
    assert_eq!(token.len(), 43, "{page}");
    assert_eq!(w.get(&format!("/share/{token}")).await.0, StatusCode::OK);

    let (_, _, recipe_page) = send(&w.app, "GET", &format!("/recipes/{}", w.recipe), Some(&w.alice), None).await;
    assert!(!recipe_page.contains(&token), "the recipe page lists the link but not its token");
    assert!(recipe_page.contains("/share-links/"), "and offers to revoke it");
    let id = w.db.share_links(&w.t.a, w.recipe).await.unwrap()[0].id;
    let (s, h, _) = send(&w.app, "POST", &format!("/recipes/{}/share-links/{id}/delete", w.recipe), Some(&w.alice), Some("")).await;
    assert_eq!((s, h["location"].to_str().unwrap()), (StatusCode::SEE_OTHER, format!("/recipes/{}", w.recipe).as_str()));
    assert_eq!(w.get(&format!("/share/{token}")).await.0, StatusCode::NOT_FOUND);
}
