//! Build-order step 15, part 4: the webhooks settings page with the new events, the group filter,
//! scheduled webhooks (time picker, default disabled), the edit page, and no secrets on screen.
mod common;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use mealier::{Db, WebhookInput};
use tower::ServiceExt;

async fn app(db: Db) -> Router {
    common::sign_in(&db).await;
    mealier::api::router(mealier::api::AppState { db, images_dir: tempfile::tempdir().unwrap().keep() })
}

async fn call(app: &Router, method: &str, uri: &str, form: Option<&str>) -> (StatusCode, HeaderMap, String) {
    let mut req = Request::builder().method(method).uri(uri).header("authorization", common::BEARER);
    let body = match form {
        Some(f) => {
            req = req.header("content-type", "application/x-www-form-urlencoded");
            Body::from(f.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let (status, headers) = (resp.status(), resp.headers().clone());
    (status, headers, String::from_utf8_lossy(&resp.into_body().collect().await.unwrap().to_bytes()).into_owned())
}

fn json_hook(url: &str) -> WebhookInput {
    WebhookInput { name: "phone".into(), kind: "json".into(), url: url.into(), events: vec!["recipe_created".into()], ntfy_tags: String::new(), enabled: true, group_id: None, household_id: None, time_of_day: None }
}

#[tokio::test]
async fn the_list_shows_a_url_as_scheme_and_host_only() {
    let db = Db::open_memory().await.unwrap();
    db.create_webhook(json_hook("https://user:hunter2@ntfy.example.com:8443/secret-topic?auth=tk_abc123")).await.unwrap();
    let (s, _, page) = call(&app(db).await, "GET", "/settings/webhooks", None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(page.contains("https://ntfy.example.com:8443"), "{page}");
    for secret in ["hunter2", "secret-topic", "tk_abc123", "user:"] {
        assert!(!page.contains(secret), "{secret} is on the page");
    }
}

#[tokio::test]
async fn the_add_form_offers_every_event_the_group_filter_and_a_time_picker() {
    let db = Db::open_memory().await.unwrap();
    db.create_group("Second").await.unwrap();
    let (_, _, page) = call(&app(db).await, "GET", "/settings/webhooks", None).await;
    for e in mealier::EVENTS {
        assert!(page.contains(&format!("name=\"ev_{e}\"")), "no checkbox for {e}");
    }
    assert!(page.contains("name=\"group_id\"") && page.contains("Second"), "{page}");
    assert!(page.contains("type=\"time\"") && page.contains("name=\"time_of_day\"") && page.contains("name=\"household_id\""), "{page}");
    assert!(page.contains("value=\"scheduled\""), "{page}");
}

#[tokio::test]
async fn creating_with_new_events_and_a_group_filter_stores_them_in_event_order() {
    let db = Db::open_memory().await.unwrap();
    let g = common::scope(&db).await.group_id;
    let a = app(db.clone()).await;
    let f = format!("name=n&kind=json&url=http%3A%2F%2Fa.test%2Fx&group_id={g}&ev_cookbook_deleted=on&ev_recipe_created=on&ev_batch_logged=on");
    let (s, _, b) = call(&a, "POST", "/settings/webhooks", Some(&f)).await;
    assert_eq!(s, StatusCode::SEE_OTHER, "{b}");
    let w = &db.list_webhooks().await.unwrap()[0];
    assert_eq!((w.events.as_slice(), w.group_id), (&["batch_logged".to_string(), "recipe_created".into(), "cookbook_deleted".into()][..], Some(g)));
    // an empty group choice means every group
    let f = "name=m&kind=json&url=http%3A%2F%2Fa.test%2Fx&group_id=&ev_recipe_created=on";
    assert_eq!(call(&a, "POST", "/settings/webhooks", Some(f)).await.0, StatusCode::SEE_OTHER);
    assert_eq!(db.list_webhooks().await.unwrap()[1].group_id, None);
}

#[tokio::test]
async fn a_scheduled_webhook_made_on_the_page_starts_disabled_unless_ticked() {
    let db = Db::open_memory().await.unwrap();
    let h = common::household(&db).await;
    let a = app(db.clone()).await;
    let f = format!("name=morning&kind=scheduled&url=http%3A%2F%2Fa.test%2Fplan&time_of_day=07%3A30&household_id={h}");
    let (s, hd, b) = call(&a, "POST", "/settings/webhooks", Some(&f)).await;
    assert_eq!((s, hd["location"].to_str().unwrap()), (StatusCode::SEE_OTHER, "/settings/webhooks"), "{b}");
    let w = db.list_webhooks().await.unwrap().remove(0);
    assert_eq!((w.kind.as_str(), w.time_of_day.as_deref(), w.household_id, w.enabled), ("scheduled", Some("07:30"), Some(h), false));
    assert!(w.events.is_empty());

    let on = format!("{f}&enabled=on");
    call(&a, "POST", "/settings/webhooks", Some(&on)).await;
    assert!(db.list_webhooks().await.unwrap()[1].enabled);

    let (_, _, page) = call(&a, "GET", "/settings/webhooks", None).await;
    assert!(page.contains("07:30") && page.contains("(disabled)"), "{page}");
    // a missing time is refused
    let (s, ..) = call(&a, "POST", "/settings/webhooks", Some(&format!("name=x&kind=scheduled&url=http%3A%2F%2Fa.test%2F&household_id={h}"))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn toggling_a_scheduled_webhook_keeps_its_time_and_household() {
    let db = Db::open_memory().await.unwrap();
    let h = common::household(&db).await;
    let w = db.create_webhook(WebhookInput { kind: "scheduled".into(), events: vec![], household_id: Some(h), time_of_day: Some("06:15".into()), enabled: false, ..json_hook("http://a.test/p") }).await.unwrap();
    let a = app(db.clone()).await;
    assert_eq!(call(&a, "POST", &format!("/settings/webhooks/{}/toggle", w.id), Some("")).await.0, StatusCode::SEE_OTHER);
    let w = db.get_webhook(w.id).await.unwrap();
    assert_eq!((w.enabled, w.time_of_day.as_deref(), w.household_id), (true, Some("06:15"), Some(h)));
}

#[tokio::test]
async fn the_edit_page_shows_the_saved_values_but_not_the_url() {
    let db = Db::open_memory().await.unwrap();
    let h = common::household(&db).await;
    let w = db.create_webhook(WebhookInput { kind: "scheduled".into(), events: vec![], household_id: Some(h), time_of_day: Some("06:15".into()), ..json_hook("https://x.example.com/hook/tk_sekret") }).await.unwrap();
    let (s, _, page) = call(&app(db).await, "GET", &format!("/settings/webhooks/{}/edit", w.id), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(page.contains("value=\"06:15\"") && page.contains("value=\"phone\""), "{page}");
    assert!(page.contains("https://x.example.com"), "{page}");
    assert!(!page.contains("tk_sekret") && !page.contains("/hook"), "the saved URL is not shown");
}

#[tokio::test]
async fn saving_the_edit_page_keeps_a_blank_url_and_replaces_a_new_one() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let w = db.create_webhook(WebhookInput { events: vec!["recipe_created".into()], ntfy_tags: "a".into(), ..json_hook("https://x.example.com/hook/tk_sekret") }).await.unwrap();
    let a = app(db.clone()).await;
    let edit = format!("/settings/webhooks/{}/edit", w.id);

    let f = format!("name=renamed&url=&ntfy_tags=b&group_id={}&enabled=on&ev_recipe_updated=on&ev_cookbook_created=on", t.a.group_id);
    let (s, hd, b) = call(&a, "POST", &edit, Some(&f)).await;
    assert_eq!((s, hd["location"].to_str().unwrap()), (StatusCode::SEE_OTHER, "/settings/webhooks"), "{b}");
    let u = db.get_webhook(w.id).await.unwrap();
    assert_eq!((u.name.as_str(), u.url.as_str(), u.ntfy_tags.as_str(), u.group_id, u.enabled), ("renamed", "https://x.example.com/hook/tk_sekret", "b", Some(t.a.group_id), true));
    assert_eq!(u.events, ["recipe_updated", "cookbook_created"]);

    let f = "name=renamed&url=https%3A%2F%2Fy.example.com%2Fnew&ev_recipe_updated=on";
    assert_eq!(call(&a, "POST", &edit, Some(f)).await.0, StatusCode::SEE_OTHER);
    let u = db.get_webhook(w.id).await.unwrap();
    assert_eq!((u.url.as_str(), u.group_id, u.enabled), ("https://y.example.com/new", None, false), "an unticked box and an empty group are saved as such");

    assert_eq!(call(&a, "GET", "/settings/webhooks/99/edit", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&a, "POST", "/settings/webhooks/99/edit", Some(f)).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn editing_a_scheduled_webhook_changes_its_time_and_household() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let w = db.create_webhook(WebhookInput { kind: "scheduled".into(), events: vec![], household_id: Some(t.a.household_id), time_of_day: Some("06:15".into()), enabled: false, ..json_hook("http://a.test/p") }).await.unwrap();
    let f = format!("name=plan&url=&time_of_day=18%3A45&household_id={}&enabled=on", t.c.household_id);
    let (s, _, b) = call(&app(db.clone()).await, "POST", &format!("/settings/webhooks/{}/edit", w.id), Some(&f)).await;
    assert_eq!(s, StatusCode::SEE_OTHER, "{b}");
    let u = db.get_webhook(w.id).await.unwrap();
    assert_eq!((u.time_of_day.as_deref(), u.household_id, u.group_id, u.enabled, u.kind.as_str()), (Some("18:45"), Some(t.c.household_id), Some(t.c.group_id), true, "scheduled"));
}

#[tokio::test]
async fn a_failed_test_send_does_not_put_the_url_in_the_message() {
    let db = Db::open_memory().await.unwrap();
    let w = db.create_webhook(json_hook("http://127.0.0.1:1/tk_sekret")).await.unwrap();
    let (s, hd, _) = call(&app(db).await, "POST", &format!("/settings/webhooks/{}/test", w.id), Some("")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    let loc = hd["location"].to_str().unwrap();
    assert!(loc.contains("delivery") && !loc.contains("sekret") && !loc.contains("127.0.0.1"), "{loc}");
}
