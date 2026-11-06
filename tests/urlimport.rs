use std::net::IpAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use axum::routing::get;
use http_body_util::BodyExt;
use mealier::fetch::Fetcher;
use mealier::{Chain, Db, Dimension, Exec, Importer, ImportService, JsonLd, Microdata, Page, api, map_jsonld};
use serde_json::{Value, json};
use tower::ServiceExt;

fn fixture(path: &str) -> String {
    std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(path)).unwrap()
}

fn plugin(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugins").join(name).to_str().unwrap().into()
}

fn page(file: &str) -> Page {
    Page { url: "https://x.test/r".into(), final_url: "https://x.test/r".into(), html: fixture(&format!("pages/{file}")) }
}

async fn mapped(file: &str) -> (mealier::RecipeInput, Option<String>) {
    let db = Db::open_memory().await.unwrap();
    db.add_unit("cup", Dimension::Volume, Some(236.588)).await.unwrap();
    db.add_food("flour").await.unwrap();
    let ld = Chain::build("jsonld,microdata", None).unwrap().run(&page(file)).await.unwrap();
    map_jsonld(&ld, "https://x.test/r", &db.units().await.unwrap(), &db.foods().await.unwrap()).unwrap()
}

// S15
#[tokio::test]
async fn plain_jsonld_maps_every_field() {
    let (r, img) = mapped("plain.html").await;
    assert_eq!(r.name, "Pancakes");
    assert_eq!(r.description, "Fluffy.");
    assert_eq!(r.source_url.as_deref(), Some("https://x.test/r"));
    assert_eq!(r.steps, ["Mix.", "Fry."]);
    assert_eq!(r.tags, ["breakfast", "easy"]);
    assert_eq!(r.categories, ["Brunch"]);
    assert_eq!(img.as_deref(), Some("https://x.test/img/pan.jpg"));
    let n = r.nutrition.unwrap();
    assert_eq!((n.calories, n.protein_g, n.fat_g, n.carbs_g), (Some(250.0), Some(7.5), Some(9.0), Some(33.0)));
    assert_eq!(r.ingredients.len(), 2);
    assert_eq!(r.ingredients[0].original_text, "2 cups flour");
    assert_eq!((r.ingredients[0].qty_min, r.ingredients[0].qty_max), (Some(2.0), Some(2.0)));
    assert!(r.ingredients[0].unit_id.is_some() && r.ingredients[0].food_id.is_some());
    assert_eq!(r.ingredients[1].original_text, "1 tsp salt, fine");
}

#[tokio::test]
async fn graph_with_array_type_and_howto_steps() {
    let (r, img) = mapped("graph.html").await;
    assert_eq!(r.name, "Graph Soup");
    assert_eq!(r.steps, ["Chop.", "Simmer for an hour."]);
    assert_eq!(img.as_deref(), Some("https://img.example/soup.png"));
}

#[tokio::test]
async fn howto_sections_are_flattened_in_order() {
    let (r, _) = mapped("sections.html").await;
    assert_eq!(r.name, "Layered Cake");
    assert_eq!(r.steps, ["Bake layers.", "Cool.", "Whip.", "Assemble."]);
}

#[tokio::test]
async fn microdata_only_page_maps_through_the_chain() {
    assert!(JsonLd.import(&page("microdata.html")).await.unwrap().is_none());
    let (r, img) = mapped("microdata.html").await;
    assert_eq!(r.name, "Micro Toast");
    assert_eq!(r.steps, ["Toast it."]);
    assert_eq!(r.ingredients.iter().map(|i| i.original_text.as_str()).collect::<Vec<_>>(), ["2 slices bread", "1 tbsp butter"]);
    assert_eq!(r.nutrition.unwrap().calories, Some(120.0));
    assert_eq!(img.as_deref(), Some("https://x.test/toast.jpg"));
}

#[tokio::test]
async fn page_without_a_recipe_yields_none() {
    assert!(Chain::build("jsonld,microdata", None).unwrap().run(&page("none.html")).await.is_none());
}

// S17
#[tokio::test]
async fn chain_falls_through_to_the_next_importer() {
    let p = page("microdata.html");
    let chain = Chain::build("jsonld,microdata", None).unwrap();
    assert_eq!(chain.0.len(), 2);
    let v = chain.run(&p).await.unwrap();
    assert_eq!(v["name"], "Micro Toast");
    assert!(Microdata.import(&p).await.unwrap().is_some());
}

#[tokio::test]
async fn chain_config_errors() {
    assert_eq!(Chain::build("jsonld,nope", None).err().unwrap(), "MEALIER_IMPORTERS: unknown importer 'nope'");
    assert!(Chain::build("exec", Some("not json")).is_err());
    let c = Chain::build("exec,jsonld", Some(r#"[["/plugins/foo","--x"]]"#)).unwrap();
    assert_eq!(c.0.len(), 2);
}

#[tokio::test]
async fn exec_returns_a_recipe_and_failure_falls_through() {
    let p = page("none.html");
    let ok = Exec::new(vec![plugin("echo-recipe")]).import(&p).await.unwrap().unwrap();
    assert_eq!(ok["name"], "From Plugin");
    assert!(Exec::new(vec![plugin("fail")]).import(&p).await.unwrap().is_none());
    let chain = Chain::build("exec,jsonld", Some(&json!([[plugin("fail")], [plugin("echo-recipe")]]).to_string())).unwrap();
    assert_eq!(chain.run(&p).await.unwrap()["name"], "From Plugin");
}

#[tokio::test]
async fn exec_timeout_is_killed() {
    let t = Instant::now();
    let r = Exec::new(vec![plugin("sleeper")]).with_timeout(Duration::from_secs(1)).import(&page("none.html")).await.unwrap();
    assert!(r.is_none());
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
}

#[tokio::test]
async fn exec_sees_only_path() {
    let v = Exec::new(vec![plugin("print-env")]).import(&page("none.html")).await.unwrap().unwrap();
    assert_eq!(v["description"], "PATH=/usr/bin:/bin;");
}

// S18
fn loopback(ip: IpAddr) -> bool {
    ip.is_loopback()
}

async fn serve() -> String {
    let app = Router::new()
        .route("/r", get(|| async { ([("content-type", "text/html")], fixture("pages/plain.html")) }))
        .route("/img/pan.jpg", get(|| async { vec![0xffu8, 0xd8, 0xff, 0xe0, 1, 2, 3] }))
        .route("/none", get(|| async { fixture("pages/none.html") }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

async fn post_import(app: &axum::Router, url: &str) -> (u16, Value) {
    let req = Request::builder().method("POST").uri("/api/recipes/import").header("content-type", "application/json").body(Body::from(json!({ "url": url }).to_string())).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    (status, serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap())
}

#[tokio::test]
async fn post_import_creates_the_recipe_and_stores_the_image() {
    let base = serve().await;
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_memory().await.unwrap();
    let svc = ImportService::new(Fetcher::with_allow(loopback), Chain::build("jsonld,microdata", None).unwrap());
    let app = api::router_with(api::AppState { db: db.clone(), images_dir: dir.path().to_path_buf() }, svc);
    let (status, body) = post_import(&app, &format!("{base}/r")).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["name"], "Pancakes");
    assert_eq!(body["source_url"], format!("{base}/r"));
    let files: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    assert_eq!(files.len(), 1);
    assert!(files[0].ends_with(".jpg"), "{files:?}");
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM recipe_images").fetch_one(db.pool()).await.unwrap();
    assert_eq!(n, 1);

    let (status, body) = post_import(&app, &format!("{base}/none")).await;
    assert_eq!((status, body["error"].as_str().unwrap()), (422, "no recipe found on that page"));
}

#[tokio::test]
async fn post_import_with_the_default_fetcher_refuses_a_private_target() {
    let base = serve().await;
    let app = api::router(api::AppState { db: Db::open_memory().await.unwrap(), images_dir: tempfile::tempdir().unwrap().keep() });
    let (status, body) = post_import(&app, &format!("{base}/r")).await;
    assert_eq!((status, body["error"].as_str().unwrap()), (422, "address is not allowed (private, loopback or internal)"));
}
