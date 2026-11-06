//! Step 14, part B: recipe actions. Links are built from Mealie's merge fields and shown only when
//! they come out as http(s); post actions send the recipe and are written by admins only.
mod common;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use mealier::{ActionInput, Db, Error, RecipeInput, Scope, render_action_url};
use serde_json::Value;

fn action(title: &str, kind: &str, url: &str) -> ActionInput {
    ActionInput { title: title.into(), kind: kind.into(), url: url.into() }
}

fn admin(sc: Scope) -> Scope {
    Scope { admin: true, ..sc }
}

fn validation<T: std::fmt::Debug>(r: Result<T, Error>, needle: &str) {
    match r {
        Err(Error::Validation(m)) => assert!(m.contains(needle), "{m:?} should mention {needle:?}"),
        other => panic!("expected a validation error mentioning {needle:?}, got {other:?}"),
    }
}

// ---------- the template ----------

#[test]
fn merge_fields_are_replaced_and_percent_encoded() {
    let got = render_action_url("https://www.google.com/search?q=${slug}&id=${id}&u=${url}", 7, "Pasta e Fagioli & more/!", "https://meals.test/recipes/7");
    assert_eq!(got.as_deref(), Some("https://www.google.com/search?q=pasta-e-fagioli-more&id=7&u=https%3A%2F%2Fmeals.test%2Frecipes%2F7"));
}

#[test]
fn a_value_cannot_break_out_of_its_place_in_the_url() {
    let got = render_action_url("https://x.test/?u=${url}", 1, "n", "https://m.test/r?a=1&b=2#frag \"q\" <b>").unwrap();
    assert!(got.starts_with("https://x.test/?u=https%3A%2F%2Fm.test%2Fr%3Fa%3D1%26b%3D2%23frag%20%22q%22%20%3Cb%3E"), "{got}");
    assert_eq!(got.matches('&').count() + got.matches('#').count() + got.matches(' ').count(), 0, "{got}");
}

#[test]
fn mealie_fields_mealier_has_no_value_for_become_empty_and_unknown_ones_stay() {
    let got = render_action_url("https://x.test/?s=${servings}&y=${yieldQuantity}${yieldText}&z=${nope}", 1, "n", "u").unwrap();
    assert_eq!(got, "https://x.test/?s=&y=&z=${nope}");
}

#[test]
fn only_http_and_https_results_are_rendered() {
    for bad in ["javascript:alert(1)", "data:text/html,x", "ftp://x.test/", "//x.test/", "/relative/${id}", "${url}", "mailto:a@b.c", "not a url", " javascript:alert(1)", "https://"] {
        assert_eq!(render_action_url(bad, 1, "n", "https://m.test/r"), None, "{bad}");
    }
    assert!(render_action_url("HTTP://x.test/${id}", 1, "n", "u").is_some());
    assert!(render_action_url("http://127.0.0.1:8123/api/webhook/${id}", 1, "n", "u").is_some());
}

// ---------- the store ----------

#[tokio::test]
async fn household_members_write_link_actions_and_validation_names_the_problem() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let a = db.create_action(&t.a, action("  Search ", "link", "https://www.google.com/search?q=${slug}")).await.unwrap();
    assert_eq!((a.title.as_str(), a.kind.as_str()), ("Search", "link"));
    assert_eq!(db.actions(&t.a).await.unwrap(), std::slice::from_ref(&a));
    validation(db.create_action(&t.a, action(" ", "link", "https://x.test/")).await, "title");
    validation(db.create_action(&t.a, action("x", "put", "https://x.test/")).await, "kind");
    validation(db.create_action(&t.a, action("x", "link", "javascript:alert(1)")).await, "http");
    validation(db.create_action(&t.a, action("x", "link", "${url}")).await, "http");
    let u = db.update_action(&t.a, a.id, action("Bring", "link", "https://api.getbring.com/rest/bringrecipes/deeplink?url=${url}&source=web")).await.unwrap();
    assert_eq!(u.title, "Bring");
    assert_eq!(db.actions(&t.a).await.unwrap().len(), 1);
    db.delete_action(&t.a, a.id).await.unwrap();
    assert!(db.actions(&t.a).await.unwrap().is_empty());
    assert!(matches!(db.delete_action(&t.a, a.id).await, Err(Error::NotFound(_))));
}

#[tokio::test]
async fn post_actions_are_written_by_admins_only() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    assert!(matches!(db.create_action(&t.a, action("Hook", "post", "http://hass.lan/api/webhook/x")).await, Err(Error::Forbidden(_))));
    assert!(db.actions(&t.a).await.unwrap().is_empty());
    let p = db.create_action(&admin(t.a), action("Hook", "post", "http://hass.lan/api/webhook/x")).await.unwrap();
    let l = db.create_action(&t.a, action("Look", "link", "https://x.test/${id}")).await.unwrap();
    // a plain member cannot edit a post action, nor turn a link into one
    assert!(matches!(db.update_action(&t.a, p.id, action("Hook2", "post", "http://evil/")).await, Err(Error::Forbidden(_))));
    assert!(matches!(db.update_action(&t.a, p.id, action("Hook2", "link", "https://x.test/")).await, Err(Error::Forbidden(_))));
    assert!(matches!(db.update_action(&t.a, l.id, action("Look", "post", "http://evil/")).await, Err(Error::Forbidden(_))));
    let all = db.actions(&t.a).await.unwrap();
    assert_eq!((all[0].url.as_str(), all[1].kind.as_str()), ("http://hass.lan/api/webhook/x", "link"), "{all:?}");
    // an admin may; and a post URL must be http(s) too
    db.update_action(&admin(t.a), p.id, action("Hook2", "post", "http://hass.lan/other")).await.unwrap();
    validation(db.create_action(&admin(t.a), action("x", "post", "ftp://x/")).await, "http");
}

#[tokio::test]
async fn another_household_cannot_see_change_or_remove_an_action() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let a = db.create_action(&t.a, action("Look", "link", "https://x.test/")).await.unwrap();
    assert!(db.actions(&t.b).await.unwrap().is_empty() && db.actions(&t.c).await.unwrap().is_empty());
    for sc in [t.b, t.c] {
        assert!(matches!(db.update_action(&sc, a.id, action("x", "link", "https://x.test/")).await, Err(Error::NotFound(_))));
        assert!(matches!(db.delete_action(&sc, a.id).await, Err(Error::NotFound(_))));
    }
    assert_eq!(db.actions(&t.a).await.unwrap()[0].title, "Look");
}

#[tokio::test]
async fn a_recipe_lists_the_households_actions_with_links_finished() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let r = db.create_recipe(&t.a, RecipeInput { name: "Tomato Soup".into(), ..Default::default() }).await.unwrap().id;
    db.create_action(&t.a, action("Search", "link", "https://www.google.com/search?q=${slug}")).await.unwrap();
    db.create_action(&admin(t.a), action("Hook", "post", "http://hass.lan/x")).await.unwrap();
    let got = db.recipe_action_links(&t.a, r).await.unwrap();
    assert_eq!(got.iter().map(|l| (l.title.as_str(), l.href.as_deref())).collect::<Vec<_>>(), [("Hook", None), ("Search", Some("https://www.google.com/search?q=tomato-soup"))]);
    assert!(db.recipe_action_links(&t.b, r).await.unwrap().is_empty(), "another household's actions are its own");
    assert!(matches!(db.recipe_action_links(&t.c, r).await, Err(Error::NotFound(_))), "a recipe of another group");
}

// ---------- posting ----------

#[derive(Clone, Default)]
struct Rx(Arc<Mutex<Vec<(HeaderMap, String)>>>, u16);

async fn receiver(status: u16) -> (String, Rx) {
    let rx = Rx(Default::default(), status);
    async fn h(State(rx): State<Rx>, headers: HeaderMap, body: String) -> StatusCode {
        rx.0.lock().unwrap().push((headers, body));
        StatusCode::from_u16(rx.1).unwrap()
    }
    let app = Router::new().route("/{*p}", post(h)).with_state(rx.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, rx)
}

#[tokio::test]
async fn triggering_a_post_action_sends_the_recipe_as_json_to_an_internal_host() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let r = db.create_recipe(&t.a, RecipeInput { name: "Soup".into(), description: "warm".into(), ..Default::default() }).await.unwrap().id;
    let (url, rx) = receiver(204).await;
    let a = db.create_action(&admin(t.a), action("Hook", "post", &format!("{url}/hook"))).await.unwrap();
    assert_eq!(db.trigger_action(&t.a, a.id, r).await.unwrap(), 204, "a plain member may trigger what an admin set up");
    let got = rx.0.lock().unwrap().clone();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0["content-type"], "application/json");
    let v: Value = serde_json::from_str(&got[0].1).unwrap();
    assert_eq!((v["id"].as_i64(), v["name"].as_str(), v["description"].as_str()), (Some(r), Some("Soup"), Some("warm")), "{v}");
}

#[tokio::test]
async fn triggering_refuses_links_other_tenants_and_failed_deliveries() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let r = db.create_recipe(&t.a, RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap().id;
    let link = db.create_action(&t.a, action("Look", "link", "https://x.test/")).await.unwrap();
    validation(db.trigger_action(&t.a, link.id, r).await, "post");
    let (url, rx) = receiver(500).await;
    let p = db.create_action(&admin(t.a), action("Hook", "post", &format!("{url}/h"))).await.unwrap();
    validation(db.trigger_action(&t.a, p.id, r).await, "500");
    let dead = db.create_action(&admin(t.a), action("Dead", "post", "http://127.0.0.1:9/x")).await.unwrap();
    validation(db.trigger_action(&t.a, dead.id, r).await, "Dead");
    let sent = rx.0.lock().unwrap().len();
    // another household's action, another group's recipe: nothing is sent
    assert!(matches!(db.trigger_action(&t.b, p.id, r).await, Err(Error::NotFound(_))));
    assert!(matches!(db.trigger_action(&t.a, p.id, 999).await, Err(Error::NotFound(_))));
    let b_action = db.create_action(&admin(t.c), action("Hook", "post", &format!("{url}/h"))).await.unwrap();
    assert!(matches!(db.trigger_action(&t.c, b_action.id, r).await, Err(Error::NotFound(_))));
    assert_eq!(rx.0.lock().unwrap().len(), sent);
}
