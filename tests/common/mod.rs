//! Shared by the HTTP tests: every route needs a user now, so this makes one.
#![allow(dead_code)]
use mealier::{Db, Scope, hash_token};

/// The `Authorization` header value of the suite's admin; request builders add it.
pub const BEARER: &str = "Bearer mlr_test-suite-token";

/// Creates the suite's admin user and API token in `db` (once; calling it again is harmless).
pub async fn sign_in(db: &Db) {
    let id: i64 = sqlx::query_scalar("INSERT INTO users (username, email, role, household_id) VALUES ('suite-admin', 'suite-admin@x', 'admin', ?) ON CONFLICT (username) DO UPDATE SET role = 'admin' RETURNING id")
        .bind(household(db).await)
        .fetch_one(db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT OR IGNORE INTO api_tokens (hash, user_id, name) VALUES (?, ?, 'suite')")
        .bind(hash_token(BEARER.strip_prefix("Bearer ").unwrap()))
        .bind(id)
        .execute(db.pool())
        .await
        .unwrap();
}

/// The household new users join (Home in Home, made if need be): where `sign_in` and the raw user inserts put people.
pub async fn household(db: &Db) -> i64 {
    db.default_household().await.unwrap().id
}

/// The tenant the store-level tests act in: the default household.
pub async fn scope(db: &Db) -> Scope {
    let h = db.default_household().await.unwrap();
    Scope { user_id: 0, group_id: h.group_id, household_id: h.id, admin: false }
}

/// Two groups and three households: `a` and `b` are different households of group One, `c` is the only household of group Two.
pub struct Tenants {
    pub a: Scope,
    pub b: Scope,
    pub c: Scope,
}

pub async fn tenants(db: &Db) -> Tenants {
    let one = db.create_group("One").await.unwrap();
    let two = db.create_group("Two").await.unwrap();
    let alpha = db.create_household(one.id, "Alpha").await.unwrap();
    let beta = db.create_household(one.id, "Beta").await.unwrap();
    let gamma = db.create_household(two.id, "Gamma").await.unwrap();
    let at = |g: i64, h: i64| Scope { user_id: 0, group_id: g, household_id: h, admin: false };
    Tenants { a: at(one.id, alpha.id), b: at(one.id, beta.id), c: at(two.id, gamma.id) }
}

/// A real, tiny, solid-colour image: the store decodes what it is given, so fake bytes no longer pass.
/// `seed` picks the colour, so two seeds are two different files.
pub fn image_bytes(format: image::ImageFormat, w: u32, h: u32, seed: u8) -> Vec<u8> {
    let img = image::RgbImage::from_pixel(w, h, image::Rgb([seed, 40, 90]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, format).unwrap();
    out.into_inner()
}

pub fn png(w: u32, h: u32, seed: u8) -> Vec<u8> {
    image_bytes(image::ImageFormat::Png, w, h, seed)
}

pub fn jpeg(w: u32, h: u32, seed: u8) -> Vec<u8> {
    image_bytes(image::ImageFormat::Jpeg, w, h, seed)
}

pub fn webp(w: u32, h: u32, seed: u8) -> Vec<u8> {
    image_bytes(image::ImageFormat::WebP, w, h, seed)
}

/// Width and height of an image file's bytes.
pub fn dims(bytes: &[u8]) -> (u32, u32) {
    image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().unwrap().into_dimensions().unwrap()
}

/// One request a test receiver got.
#[derive(Clone)]
pub struct Hit {
    pub path: String,
    pub headers: axum::http::HeaderMap,
    pub body: String,
}

/// A receiver on 127.0.0.1 that records every POST and answers `status`. Returns its base URL.
pub async fn receiver(status: u16) -> (String, std::sync::Arc<std::sync::Mutex<Vec<Hit>>>) {
    use axum::extract::State;
    type Hits = std::sync::Arc<std::sync::Mutex<Vec<Hit>>>;
    async fn h(State((hits, status)): State<(Hits, u16)>, uri: axum::http::Uri, headers: axum::http::HeaderMap, body: String) -> axum::http::StatusCode {
        hits.lock().unwrap().push(Hit { path: uri.path().into(), headers, body });
        axum::http::StatusCode::from_u16(status).unwrap()
    }
    let hits: Hits = Default::default();
    let app = axum::Router::new().route("/{*p}", axum::routing::post(h)).with_state((hits.clone(), status));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, hits)
}

/// Waits (up to 3 s) for `n` requests, then a beat more so extras would show.
pub async fn hits(h: &std::sync::Arc<std::sync::Mutex<Vec<Hit>>>, n: usize) -> Vec<Hit> {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while h.lock().unwrap().len() < n && std::time::Instant::now() < end {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    h.lock().unwrap().clone()
}
