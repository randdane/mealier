use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use mealier::{Db, Dimension, IngredientInput, RecipeInput, api};
use tower::ServiceExt;

async fn request(app: &axum::Router, method: &str, uri: &str, form: Option<&str>) -> (StatusCode, Option<String>, String) {
    let mut req = Request::builder().method(method).uri(uri);
    let body = match form {
        Some(f) => {
            req = req.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
            Body::from(f.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let location = resp.headers().get(header::LOCATION).map(|v| v.to_str().unwrap().to_string());
    let text = String::from_utf8(resp.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    (status, location, text)
}

fn app(db: Db) -> axum::Router {
    api::router(api::AppState { db, images_dir: tempfile::tempdir().unwrap().keep() })
}

#[tokio::test]
async fn review_form_post_updates_the_recipe_and_creates_a_new_food_and_unit() {
    let db = Db::open_memory().await.unwrap();
    let cup = db.add_unit("cup", Dimension::Volume, Some(236.588)).await.unwrap();
    let flour = db.add_food("flour").await.unwrap();
    let recipe = db
        .create_recipe(RecipeInput {
            name: "Mix".into(),
            ingredients: vec![
                IngredientInput { original_text: "2 cups flour".into(), ..Default::default() },
                IngredientInput { original_text: "3 oz zorblax".into(), ..Default::default() },
                IngredientInput { original_text: "a pinch of mace".into(), ..Default::default() },
            ],
            ..Default::default()
        })
        .await
        .unwrap();
    let app = app(db.clone());

    // Row 1 picks existing ones. Row 2 adds a new food and a new mass unit. Row 3 adds a new count unit.
    let form = format!(
        "original_text=2+cups+flour&qty_min=2&qty_max=2&unit_id={cup}&new_unit=&new_unit_dimension=count&new_unit_factor=&food_id={flour}&new_food=&note=\
         &original_text=3+oz+zorblax&qty_min=3&qty_max=3&unit_id=&new_unit=ounce&new_unit_dimension=mass&new_unit_factor=28.35&food_id=&new_food=zorblax&note=fresh\
         &original_text=a+pinch+of+mace&qty_min=&qty_max=&unit_id=&new_unit=pinch&new_unit_dimension=count&new_unit_factor=&food_id=&new_food=mace&note="
    );
    let (status, location, _) = request(&app, "POST", &format!("/recipes/{}/ingredients/review", recipe.id), Some(&form)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.unwrap(), format!("/recipes/{}", recipe.id));

    let got = db.get_recipe(recipe.id).await.unwrap().unwrap();
    assert_ne!(got.current_snapshot_id, recipe.current_snapshot_id, "a new snapshot was written");
    assert_eq!(db.snapshots(recipe.id).await.unwrap().len(), 2);

    let foods = db.foods().await.unwrap();
    let zorblax = foods.iter().find(|f| f.name == "zorblax").expect("new food created").id;
    let mace = foods.iter().find(|f| f.name == "mace").unwrap().id;
    let units = db.units().await.unwrap();
    let ounce = units.iter().find(|u| u.name == "ounce").expect("new unit created");
    assert_eq!((ounce.dimension, ounce.factor), (Dimension::Mass, Some(28.35)));
    let pinch = units.iter().find(|u| u.name == "pinch").unwrap();
    assert_eq!((pinch.dimension, pinch.factor), (Dimension::Count, None));

    let i = &got.content.ingredients;
    assert_eq!((i[0].food_id, i[0].unit_id, i[0].qty_min), (Some(flour), Some(cup), Some(2.0)));
    assert_eq!((i[1].food_id, i[1].unit_id, i[1].qty_max, i[1].note.as_str()), (Some(zorblax), Some(ounce.id), Some(3.0), "fresh"));
    assert_eq!(i[1].original_text, "3 oz zorblax");
    assert_eq!((i[2].food_id, i[2].unit_id, i[2].qty_min), (Some(mace), Some(pinch.id), None));
}

#[tokio::test]
async fn new_recipe_form_parses_lines_and_redirects_to_the_review_screen() {
    let db = Db::open_memory().await.unwrap();
    db.add_food("flour").await.unwrap();
    let app = app(db.clone());

    let (status, location, _) = request(&app, "POST", "/recipes", Some("name=Bread&description=&ingredients=2+flour%0D%0A%0D%0Anonsense+line%0D%0A")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let location = location.unwrap();
    assert!(location.starts_with("/recipes/") && location.ends_with("/ingredients/review"), "{location}");

    let id: i64 = location.split('/').nth(2).unwrap().parse().unwrap();
    let r = db.get_recipe(id).await.unwrap().unwrap();
    assert_eq!(r.content.ingredients.len(), 2, "blank lines are dropped");
    assert_eq!(r.content.ingredients[0].food_name.as_deref(), Some("flour"));
    assert_eq!(r.content.ingredients[1].original_text, "nonsense line");

    let (status, _, html) = request(&app, "GET", &location, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("nonsense line"));
}
