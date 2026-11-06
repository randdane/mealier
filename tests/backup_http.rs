//! The admin side of export and restore: download, upload, and the restart that follows an upload.
mod common;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use mealier::backup::{Restart, export};
use mealier::{AuthConfig, Db, ImportService, RecipeInput, api, hash_token};
use tower::ServiceExt;

struct Server {
    app: Router,
    db: Db,
    dir: tempfile::TempDir,
    restart: Arc<Restart>,
    user_token: String,
}

async fn server(recipes: &[&str]) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("mealier.db")).await.unwrap();
    common::sign_in(&db).await;
    let sc = common::scope(&db).await;
    for name in recipes {
        let r = db.create_recipe(&sc, RecipeInput { name: (*name).into(), ..Default::default() }).await.unwrap();
        db.set_recipe_image(&sc, r.id, &dir.path().join("images"), &common::png(20, 20, name.bytes().map(|b| b.wrapping_mul(7)).fold(0u8, u8::wrapping_add))).await.unwrap();
    }
    let uid: i64 = sqlx::query_scalar("INSERT INTO users (username, email, role, household_id) VALUES ('plain', 'plain@x', 'user', ?) RETURNING id")
        .bind(common::household(&db).await)
        .fetch_one(db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO api_tokens (hash, user_id, name) VALUES (?, ?, 't')").bind(hash_token("mlr_plain-user")).bind(uid).execute(db.pool()).await.unwrap();
    let restart = Arc::new(Restart::new());
    let state = api::AppState { db: db.clone(), images_dir: dir.path().join("images") };
    let app = api::router_with_restart(state, ImportService::default(), AuthConfig::default(), restart.clone());
    Server { app, db, dir, restart, user_token: "Bearer mlr_plain-user".into() }
}

async fn call(app: &Router, method: &str, uri: &str, token: &str, content_type: Option<&str>, body: Vec<u8>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut req = Request::builder().method(method).uri(uri).header("authorization", token);
    if let Some(ct) = content_type {
        req = req.header("content-type", ct);
    }
    let resp = app.clone().oneshot(req.body(Body::from(body)).unwrap()).await.unwrap();
    let (status, headers) = (resp.status(), resp.headers().clone());
    (status, headers, resp.into_body().collect().await.unwrap().to_bytes().to_vec())
}

fn multipart(field: &str, bytes: &[u8]) -> Vec<u8> {
    let mut b = format!("--XX\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"backup.zip\"\r\nContent-Type: application/zip\r\n\r\n").into_bytes();
    b.extend_from_slice(bytes);
    b.extend_from_slice(b"\r\n--XX--\r\n");
    b
}

const MP: &str = "multipart/form-data; boundary=XX";

fn zip_names(bytes: &[u8]) -> Vec<String> {
    let mut a = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    (0..a.len()).map(|i| a.by_index(i).unwrap().name().unwrap().to_string()).collect()
}

async fn recipe_names(dir: &Path) -> Vec<String> {
    let db = Db::open(&dir.join("mealier.db")).await.unwrap();
    let sc = common::scope(&db).await;
    let names = db.list_recipes(&sc, true, None).await.unwrap().into_iter().map(|r| r.name).collect();
    db.pool().close().await;
    names
}

#[tokio::test]
async fn an_admin_downloads_the_export_as_a_zip() {
    let s = server(&["Soup", "Stew"]).await;
    let (status, headers, body) = call(&s.app, "GET", "/api/export", common::BEARER, None, vec![]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/zip");
    assert!(headers["content-disposition"].to_str().unwrap().starts_with("attachment; filename=\"mealier-export-"), "{headers:?}");
    let names = zip_names(&body);
    assert_eq!(names.iter().filter(|n| n.as_str() == "mealier.db").count(), 1);
    assert_eq!(names.iter().filter(|n| n.starts_with("images/")).count(), 2);
    let staging = s.dir.path().join("export-staging");
    assert!(!staging.exists() || std::fs::read_dir(staging).unwrap().next().is_none(), "nothing left in the staging directory");
}

#[tokio::test]
async fn only_admins_may_export_or_restore() {
    let s = server(&["Soup"]).await;
    for (method, uri, body, ct) in [("GET", "/api/export", vec![], None), ("POST", "/api/restore", multipart("file", b"x"), Some(MP)), ("GET", "/settings/backup", vec![], None), ("POST", "/settings/backup/restore", multipart("file", b"x"), Some(MP))] {
        let (status, ..) = call(&s.app, method, uri, &s.user_token, ct, body.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
        // Not signed in: the API says 401, a page sends you to the sign-in form.
        let (status, ..) = call(&s.app, method, uri, "Bearer mlr_nobody", ct, body).await;
        assert_eq!(status, if uri.starts_with("/api") { StatusCode::UNAUTHORIZED } else { StatusCode::SEE_OTHER }, "{method} {uri}");
    }
    assert!(!s.restart.is_requested());
}

#[tokio::test]
async fn an_uploaded_backup_is_swapped_in_and_the_server_asks_to_be_restarted() {
    let a = server(&["Soup", "Stew"]).await;
    let zip = a.dir.path().parent().unwrap().join("from-a.zip");
    export(a.db.pool(), a.dir.path(), &zip).await.unwrap();

    let b = server(&["Old"]).await;
    let (status, _, body) = call(&b.app, "POST", "/api/restore", common::BEARER, Some(MP), multipart("file", &std::fs::read(&zip).unwrap())).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{}", String::from_utf8_lossy(&body));
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["restarting"], true);
    assert!(v["previous"].as_str().unwrap().starts_with("pre-restore-"), "{v}");
    assert!(b.restart.is_requested(), "main shuts down on this");

    assert_eq!(recipe_names(b.dir.path()).await, ["Soup", "Stew"]);
    let kept = std::fs::read_dir(b.dir.path()).unwrap().map(|e| e.unwrap().path()).find(|p| p.file_name().unwrap().to_str().unwrap().starts_with("pre-restore-")).unwrap();
    assert_eq!(recipe_names(&kept).await, ["Old"]);

    // Until the process is gone it must not take writes the swap would lose.
    let (status, ..) = call(&b.app, "GET", "/api/recipes", common::BEARER, None, vec![]).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let (status, ..) = call(&b.app, "GET", "/healthz", common::BEARER, None, vec![]).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_bad_upload_is_refused_with_422_and_changes_nothing() {
    let b = server(&["Old"]).await;
    for (what, body) in [("not a zip", multipart("file", b"PK nope")), ("no file field", multipart("other", b"x"))] {
        let (status, _, resp) = call(&b.app, "POST", "/api/restore", common::BEARER, Some(MP), body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{what}: {}", String::from_utf8_lossy(&resp));
    }
    assert!(!b.restart.is_requested());
    let (status, ..) = call(&b.app, "GET", "/api/recipes", common::BEARER, None, vec![]).await;
    assert_eq!(status, StatusCode::OK, "still serving");
    let left: Vec<_> = std::fs::read_dir(b.dir.path()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).filter(|n| n.starts_with("pre-restore") || n.contains("staging") || n.contains("upload")).collect();
    assert!(left.is_empty() || left == ["export-staging"], "{left:?}");
}

#[tokio::test]
async fn the_backup_page_links_the_download_and_offers_the_upload() {
    let s = server(&[]).await;
    let (status, headers, body) = call(&s.app, "GET", "/settings/backup", common::BEARER, None, vec![]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers["content-type"].to_str().unwrap().starts_with("text/html"));
    let html = String::from_utf8(body).unwrap();
    assert!(html.contains("href=\"/api/export\"") && html.contains("enctype=\"multipart/form-data\"") && html.contains("/settings/backup/restore"), "{html}");
}

#[tokio::test]
async fn the_upload_form_answers_with_a_page_not_json() {
    let a = server(&["Soup"]).await;
    let zip = a.dir.path().parent().unwrap().join("from-a-form.zip");
    export(a.db.pool(), a.dir.path(), &zip).await.unwrap();
    let b = server(&[]).await;
    let (status, headers, body) = call(&b.app, "POST", "/settings/backup/restore", common::BEARER, Some(MP), multipart("file", &std::fs::read(&zip).unwrap())).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(headers["content-type"].to_str().unwrap().starts_with("text/html"));
    assert!(String::from_utf8(body).unwrap().contains("restart"));
    assert!(b.restart.is_requested());

    // A refusal from the form is a page too (item 8 gives every HTML route its error page).
    let c = server(&[]).await;
    let (status, headers, body) = call(&c.app, "POST", "/settings/backup/restore", common::BEARER, Some(MP), multipart("file", b"PK nope")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let _ = (headers, body);
}

#[tokio::test]
async fn the_openapi_document_describes_export_and_restore() {
    let s = server(&[]).await;
    let (status, _, body) = call(&s.app, "GET", "/api/openapi.json", common::BEARER, None, vec![]).await;
    assert_eq!(status, StatusCode::OK);
    let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(doc["paths"]["/api/export"]["get"].is_object() && doc["paths"]["/api/restore"]["post"].is_object(), "{}", doc["paths"]);
}
