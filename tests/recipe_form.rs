//! The HTML recipe form covers every field the recipe has: steps, tags, categories, tools, cookbooks,
//! nutrition and an image. It posts as multipart (for the image) but a plain urlencoded post still works.
mod common;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use mealier::{Db, RecipeInput, api};
use tower::ServiceExt;

struct Site {
    app: Router,
    db: Db,
    images: std::path::PathBuf,
}

async fn site() -> Site {
    let db = Db::open_memory().await.unwrap();
    common::sign_in(&db).await;
    let images = tempfile::tempdir().unwrap().keep();
    Site { app: api::router(api::AppState { db: db.clone(), images_dir: images.clone() }), db, images }
}

enum Part<'a> {
    Text(&'a str, &'a str),
    File(&'a str, &'a [u8]),
}

fn multipart(parts: &[Part]) -> Vec<u8> {
    let mut b = Vec::new();
    for p in parts {
        match p {
            Part::Text(name, value) => b.extend_from_slice(format!("--XX\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes()),
            Part::File(name, bytes) => {
                b.extend_from_slice(format!("--XX\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{}\"\r\nContent-Type: application/octet-stream\r\n\r\n", if bytes.is_empty() { "" } else { "pic" }).as_bytes());
                b.extend_from_slice(bytes);
                b.extend_from_slice(b"\r\n");
            }
        }
    }
    b.extend_from_slice(b"--XX--\r\n");
    b
}

async fn send(app: &Router, method: &str, uri: &str, content_type: Option<&str>, body: Vec<u8>) -> (StatusCode, Option<String>, String) {
    let mut req = Request::builder().method(method).uri(uri).header("authorization", common::BEARER);
    if let Some(ct) = content_type {
        req = req.header(header::CONTENT_TYPE, ct);
    }
    let resp = app.clone().oneshot(req.body(Body::from(body)).unwrap()).await.unwrap();
    let (status, location) = (resp.status(), resp.headers().get(header::LOCATION).map(|v| v.to_str().unwrap().to_string()));
    (status, location, String::from_utf8_lossy(&resp.into_body().collect().await.unwrap().to_bytes()).into_owned())
}

async fn post_form(app: &Router, uri: &str, form: &str) -> (StatusCode, Option<String>, String) {
    send(app, "POST", uri, Some("application/x-www-form-urlencoded"), form.as_bytes().to_vec()).await
}

async fn post_multipart(app: &Router, uri: &str, parts: &[Part<'_>]) -> (StatusCode, Option<String>, String) {
    send(app, "POST", uri, Some("multipart/form-data; boundary=XX"), multipart(parts)).await
}

fn recipe_id(location: &str) -> i64 {
    location.strip_prefix("/recipes/").unwrap().split('/').next().unwrap().parse().unwrap()
}

const FULL: &str = "name=Soup&description=Warm&ingredients=2+onions&steps=Chop+the+onion%0D%0ASimmer+20+min%0D%0A%0D%0AServe&tags=easy%2C+dinner%2C%2C+easy&categories=Mains&tools=pot%2C+wok&cookbooks=Weeknight&calories=250&protein_g=7&fat_g=&carbs_g=33";

// ---------- create ----------

#[tokio::test]
async fn creating_with_the_whole_form_saves_every_field_in_the_first_snapshot() {
    let s = site().await;
    let (status, location, _) = post_form(&s.app, "/recipes", FULL).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let id = recipe_id(&location.unwrap());

    let sc = common::scope(&s.db).await;
    let snaps = s.db.snapshots(&sc, id).await.unwrap();
    assert_eq!(snaps.len(), 1);
    let c = s.db.snapshot(&sc, snaps[0].id).await.unwrap().content;
    assert_eq!(c.steps, ["Chop the onion", "Simmer 20 min", "Serve"], "one per line, blank lines dropped");
    assert_eq!(c.tags, ["easy", "dinner"], "comma separated, trimmed, no empties, no repeats");
    assert_eq!((c.categories.as_slice(), c.tools.as_slice(), c.cookbooks.as_slice()), (&["Mains".to_string()][..], &["pot".to_string(), "wok".to_string()][..], &["Weeknight".to_string()][..]));
    let n = c.nutrition.unwrap();
    assert_eq!((n.calories, n.protein_g, n.fat_g, n.carbs_g), (Some(250.0), Some(7.0), None, Some(33.0)));
}

#[tokio::test]
async fn creating_with_a_multipart_form_and_an_image_gives_one_snapshot_that_lists_the_resized_image() {
    let s = site().await;
    let big = common::png(3000, 1500, 4);
    let (status, location, body) = post_multipart(
        &s.app,
        "/recipes",
        &[Part::Text("name", "Soup"), Part::Text("steps", "Chop\nSimmer"), Part::Text("tags", "easy"), Part::File("image", &big)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    let id = recipe_id(&location.unwrap());

    let sc = common::scope(&s.db).await;
    assert_eq!(s.db.snapshots(&sc, id).await.unwrap().len(), 1);
    let r = s.db.get_recipe(&sc, id).await.unwrap().unwrap();
    assert_eq!((r.content.steps.as_slice(), r.content.tags.as_slice()), (&["Chop".to_string(), "Simmer".to_string()][..], &["easy".to_string()][..]));
    assert_eq!(r.content.images.len(), 1);
    let img = s.db.recipe_images(&sc, id).await.unwrap().remove(0);
    assert_eq!(common::dims(&std::fs::read(s.images.join(img.file_name())).unwrap()), (2048, 1024));
    assert_eq!(r.content.images, [img.hash]);
}

#[tokio::test]
async fn a_multipart_form_with_no_file_chosen_creates_a_recipe_without_an_image() {
    let s = site().await;
    let (status, location, body) = post_multipart(&s.app, "/recipes", &[Part::Text("name", "Soup"), Part::File("image", b"")]).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    let id = recipe_id(&location.unwrap());
    assert!(s.db.recipe_images(&common::scope(&s.db).await, id).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_bad_image_refuses_the_whole_form() {
    let s = site().await;
    let (status, _, body) = post_multipart(&s.app, "/recipes", &[Part::Text("name", "Soup"), Part::File("image", b"\x89PNG\r\n\x1a\nnot really")]).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("image"), "{body}");
    assert!(s.db.list_recipes(&common::scope(&s.db).await, true, None).await.unwrap().is_empty(), "no half-made recipe");
}

// ---------- edit ----------

async fn a_recipe(s: &Site) -> i64 {
    let (_, location, _) = post_form(&s.app, "/recipes", FULL).await;
    recipe_id(&location.unwrap())
}

#[tokio::test]
async fn editing_writes_a_new_snapshot_with_the_new_steps_and_tags_and_keeps_the_old_one() {
    let s = site().await;
    let id = a_recipe(&s).await;
    let (status, ..) = post_form(&s.app, &format!("/recipes/{id}/edit"), "name=Soup&description=Warm&ingredients=2+onions&steps=Boil&tags=quick&categories=&tools=kettle&cookbooks=").await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let sc = common::scope(&s.db).await;
    let snaps = s.db.snapshots(&sc, id).await.unwrap();
    assert_eq!(snaps.len(), 2);
    let (new, old) = (s.db.snapshot(&sc, snaps[0].id).await.unwrap().content, s.db.snapshot(&sc, snaps[1].id).await.unwrap().content);
    assert_eq!((new.steps.as_slice(), new.tags.as_slice(), new.tools.as_slice()), (&["Boil".to_string()][..], &["quick".to_string()][..], &["kettle".to_string()][..]));
    assert!(new.categories.is_empty() && new.cookbooks.is_empty(), "a blank field clears it");
    assert_eq!(old.steps, ["Chop the onion", "Simmer 20 min", "Serve"]);
    assert_eq!(old.tags, ["easy", "dinner"]);
}

#[tokio::test]
async fn a_form_that_leaves_the_new_fields_out_changes_none_of_them() {
    let s = site().await;
    let id = a_recipe(&s).await;
    let before = s.db.get_recipe(&common::scope(&s.db).await, id).await.unwrap().unwrap().content;
    post_form(&s.app, &format!("/recipes/{id}/edit"), "name=Soup+2&description=Warm&ingredients=2+onions").await;
    let after = s.db.get_recipe(&common::scope(&s.db).await, id).await.unwrap().unwrap().content;
    assert_eq!(after.name, "Soup 2");
    assert_eq!((after.steps, after.tags, after.categories, after.tools, after.cookbooks, after.nutrition), (before.steps, before.tags, before.categories, before.tools, before.cookbooks, before.nutrition));
}

#[tokio::test]
async fn editing_with_an_image_replaces_it_in_the_same_snapshot_and_editing_without_keeps_it() {
    let s = site().await;
    let id = a_recipe(&s).await;
    let sc = common::scope(&s.db).await;
    post_multipart(&s.app, &format!("/recipes/{id}/edit"), &[Part::Text("name", "Soup"), Part::File("image", &common::png(50, 40, 1))]).await;
    let first = s.db.get_recipe(&sc, id).await.unwrap().unwrap();
    assert_eq!(first.content.images.len(), 1);
    assert_eq!(s.db.snapshots(&sc, id).await.unwrap().len(), 2, "create, then the edit with the image: one snapshot each");

    post_multipart(&s.app, &format!("/recipes/{id}/edit"), &[Part::Text("name", "Soup 3"), Part::File("image", b"")]).await;
    let kept = s.db.get_recipe(&sc, id).await.unwrap().unwrap();
    assert_eq!(kept.content.images, first.content.images);

    post_multipart(&s.app, &format!("/recipes/{id}/edit"), &[Part::Text("name", "Soup 4"), Part::File("image", &common::png(50, 40, 2))]).await;
    let replaced = s.db.get_recipe(&sc, id).await.unwrap().unwrap();
    assert_ne!(replaced.content.images, first.content.images);
    assert_eq!(s.db.recipe_images(&sc, id).await.unwrap().len(), 1);
    let snaps = s.db.snapshots(&sc, id).await.unwrap();
    assert_eq!(s.db.snapshot(&sc, snaps[1].id).await.unwrap().content.images, first.content.images, "the old snapshot still lists the old image");
}

// ---------- the pages ----------

#[tokio::test]
async fn the_new_recipe_page_has_a_field_for_everything() {
    let s = site().await;
    let (status, _, html) = send(&s.app, "GET", "/recipes/new", None, vec![]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("enctype=\"multipart/form-data\""), "{html}");
    for field in ["name", "description", "ingredients", "steps", "tags", "categories", "tools", "cookbooks", "calories", "protein_g", "fat_g", "carbs_g"] {
        assert!(html.contains(&format!("name=\"{field}\"")), "no {field} field: {html}");
    }
    assert!(html.contains("name=\"image\"") && html.contains("type=\"file\""), "{html}");
}

#[tokio::test]
async fn the_edit_page_is_filled_in_with_what_the_recipe_has() {
    let s = site().await;
    let id = a_recipe(&s).await;
    let (_, _, html) = send(&s.app, "GET", &format!("/recipes/{id}/edit"), None, vec![]).await;
    assert!(html.contains("Chop the onion\nSimmer 20 min\nServe"), "{html}");
    assert!(html.contains("value=\"easy, dinner\"") && html.contains("value=\"Mains\"") && html.contains("value=\"pot, wok\"") && html.contains("value=\"Weeknight\""), "{html}");
}

// ---------- the vocabulary ----------

#[tokio::test]
async fn names_are_created_in_the_callers_group_once() {
    let s = site().await;
    post_form(&s.app, "/recipes", "name=A&tools=pot%2C+wok&tags=easy&categories=Mains&cookbooks=Weeknight").await;
    post_form(&s.app, "/recipes", "name=B&tools=wok%2C+whisk&tags=easy").await;
    let sc = common::scope(&s.db).await;
    for (table, want) in [("tools", vec!["pot", "whisk", "wok"]), ("tags", vec!["easy"]), ("categories", vec!["Mains"]), ("cookbooks", vec!["Weeknight"])] {
        let got: Vec<String> = sqlx::query_scalar(&format!("SELECT name FROM {table} WHERE group_id = ? ORDER BY name")).bind(sc.group_id).fetch_all(s.db.pool()).await.unwrap();
        assert_eq!(got, want, "{table}");
    }

    // another group's tools are its own: saving the same names there makes them there too, and touches nothing here
    let t = common::tenants(&s.db).await;
    s.db.create_recipe(&t.c, RecipeInput { name: "C".into(), tools: vec!["wok".into()], ..Default::default() }).await.unwrap();
    let theirs: Vec<String> = sqlx::query_scalar("SELECT name FROM tools WHERE group_id = ?").bind(t.c.group_id).fetch_all(s.db.pool()).await.unwrap();
    assert_eq!(theirs, ["wok"]);
    let ours: i64 = sqlx::query_scalar("SELECT count(*) FROM tools WHERE group_id = ?").bind(sc.group_id).fetch_one(s.db.pool()).await.unwrap();
    assert_eq!(ours, 3);
}
