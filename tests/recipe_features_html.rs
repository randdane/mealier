//! Step 14, part A, in the browser: the recipe form (sections and step titles as `## ` lines, repeatable
//! rows for notes, extras and substitutions, the settings), the "Link ingredients" page, the recipe
//! page, cook mode, attachments, and shopping-list extras.
mod common;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use mealier::{Db, IngredientInput, LineDetail, Note, RecipeInput, Scope, Settings, StepDetail, Substitution, SubstitutionInput, api};
use tower::ServiceExt;

struct Site {
    app: Router,
    db: Db,
    sc: Scope,
}

async fn site() -> Site {
    let db = Db::open_memory().await.unwrap();
    common::sign_in(&db).await;
    let sc = common::scope(&db).await;
    Site { app: api::router(api::AppState { db: db.clone(), images_dir: tempfile::tempdir().unwrap().keep() }), db, sc }
}

async fn send(app: &Router, method: &str, uri: &str, content_type: Option<&str>, body: Vec<u8>) -> (StatusCode, Option<String>, String) {
    let mut req = Request::builder().method(method).uri(uri).header("authorization", common::BEARER);
    if let Some(ct) = content_type {
        req = req.header(header::CONTENT_TYPE, ct);
    }
    let resp = app.clone().oneshot(req.body(Body::from(body)).unwrap()).await.unwrap();
    let (s, loc) = (resp.status(), resp.headers().get(header::LOCATION).map(|v| v.to_str().unwrap().to_string()));
    (s, loc, String::from_utf8_lossy(&resp.into_body().collect().await.unwrap().to_bytes()).into_owned())
}

async fn get(app: &Router, uri: &str) -> (StatusCode, String) {
    let (s, _, b) = send(app, "GET", uri, None, vec![]).await;
    (s, b)
}

async fn post_form(app: &Router, uri: &str, form: &str) -> (StatusCode, Option<String>, String) {
    send(app, "POST", uri, Some("application/x-www-form-urlencoded"), form.as_bytes().to_vec()).await
}

fn enc(pairs: &[(&str, &str)]) -> String {
    serde_urlencoded::to_string(pairs).unwrap()
}

fn line(t: &str) -> IngredientInput {
    IngredientInput { original_text: t.into(), ..Default::default() }
}

/// Soup: two lines (Base section on the first), three steps, the first two linked.
async fn soup(s: &Site) -> mealier::Recipe {
    let r = s.db.create_recipe(&s.sc, RecipeInput { name: "Soup".into(), ingredients: vec![line("2 onions"), line("1 tbsp oil")], steps: vec!["Chop".into(), "Fry".into(), "Serve".into()], ..Default::default() }).await.unwrap();
    let (a, b) = (r.content.ingredients[0].uid.clone(), r.content.ingredients[1].uid.clone());
    let input = RecipeInput {
        ingredient_details: vec![
            LineDetail { uid: Some(a.clone()), section: "Base".into(), substitutions: vec![Substitution { qty: Some(1.5), unit: Some("tbsp".into()), food: Some("shallots".into()), note: "milder".into() }] },
            LineDetail { uid: Some(b.clone()), ..Default::default() },
        ],
        step_details: vec![StepDetail { title: "Prep".into(), ingredient_uids: vec![a.clone()] }, StepDetail { title: String::new(), ingredient_uids: vec![a, b] }],
        ..mealier::RecipeInput::from(r.content.clone())
    };
    s.db.update_recipe(&s.sc, r.id, input).await.unwrap()
}

// ---------- the form ----------

#[tokio::test]
async fn a_hash_line_starts_a_section_in_the_ingredients_and_titles_a_step() {
    let s = site().await;
    let form = enc(&[("name", "Soup"), ("ingredients", "## Base\r\n2 onions\r\n1 tbsp oil\r\n## Sauce\r\n1 cup stock"), ("steps", "## Prep\r\nChop\r\nFry\r\n## Finish\r\nServe")]);
    let (status, loc, body) = post_form(&s.app, "/recipes", &form).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    let id: i64 = loc.unwrap().strip_prefix("/recipes/").unwrap().split('/').next().unwrap().parse().unwrap();
    let c = s.db.get_recipe(&s.sc, id).await.unwrap().unwrap().content;
    assert_eq!(c.ingredients.iter().map(|i| i.section.as_str()).collect::<Vec<_>>(), ["Base", "", "Sauce"]);
    assert_eq!(c.steps, ["Chop", "Fry", "Serve"]);
    assert_eq!(c.step_details.iter().map(|d| d.title.as_str()).collect::<Vec<_>>(), ["Prep", "", "Finish"]);

    // and the edit page writes them back as `## ` lines
    let (_, page) = get(&s.app, &format!("/recipes/{id}/edit")).await;
    assert!(page.contains("## Base\n2 onions\n1 tbsp oil\n## Sauce\n1 cup stock"), "{page}");
    assert!(page.contains("## Prep\nChop\nFry\n## Finish\nServe"), "{page}");

    let dangling = enc(&[("name", "Soup"), ("ingredients", "2 onions\n## Sauce")]);
    let (status, _, body) = post_form(&s.app, "/recipes", &dangling).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("section"), "{body}");
}

#[tokio::test]
async fn an_edit_that_leaves_lines_and_steps_alone_keeps_their_ids_substitutions_and_links() {
    let s = site().await;
    let r = soup(&s).await;
    let before = r.content.clone();
    // the page text of the recipe, with only the name changed; no `features` marker, so the rows stay as they are
    let (_, page) = get(&s.app, &format!("/recipes/{}/edit", r.id)).await;
    assert!(page.contains("## Base\n2 onions\n1 tbsp oil"), "{page}");
    let form = enc(&[("name", "Soup 2"), ("ingredients", "## Base\n2 onions\n1 tbsp oil"), ("steps", "## Prep\nChop\nFry\nServe")]);
    let (status, _, body) = post_form(&s.app, &format!("/recipes/{}/edit", r.id), &form).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    let now = s.db.get_recipe(&s.sc, r.id).await.unwrap().unwrap().content;
    assert_eq!(now.name, "Soup 2");
    assert_eq!(now.ingredients.iter().map(|i| i.uid.clone()).collect::<Vec<_>>(), before.ingredients.iter().map(|i| i.uid.clone()).collect::<Vec<_>>());
    assert_eq!(now.ingredients[0].substitutions, before.ingredients[0].substitutions);
    assert_eq!(now.step_details[1].ingredient_uids, before.step_details[1].ingredient_uids, "a step with the same text keeps its links");
    assert_eq!(now.step_details[0].title, "Prep", "titles come from the text");

    // a removed line drops out of the links, and a reworded step loses its links
    let form = enc(&[("name", "Soup 2"), ("ingredients", "## Base\n2 onions"), ("steps", "Chop finely\nFry")]);
    post_form(&s.app, &format!("/recipes/{}/edit", r.id), &form).await;
    let now = s.db.get_recipe(&s.sc, r.id).await.unwrap().unwrap().content;
    assert!(now.step_details[0].ingredient_uids.is_empty(), "reworded");
    assert_eq!(now.step_details[1].ingredient_uids, [before.ingredients[0].uid.clone()], "the removed line is gone from the links");
}

#[tokio::test]
async fn the_feature_rows_replace_notes_extras_substitutions_and_settings_and_the_lock_works_from_the_form() {
    let s = site().await;
    let r = soup(&s).await;
    let uri = format!("/recipes/{}/edit", r.id);
    let base = [("name", "Soup"), ("ingredients", "## Base\n2 onions\n1 tbsp oil"), ("steps", "## Prep\nChop\nFry\nServe"), ("features", "1")];
    let mut f: Vec<(&str, &str)> = base.to_vec();
    f.extend([
        ("note_title", "Tip"), ("note_text", "Salt late"), ("note_title", ""), ("note_text", ""),
        ("extra_key", "source"), ("extra_value", "gran"), ("extra_key", ""), ("extra_value", ""),
        ("sub_line", "2"), ("sub_qty", "2"), ("sub_unit", "tbsp"), ("sub_food", "butter"), ("sub_note", "if no oil"),
        ("sub_line", ""), ("sub_qty", ""), ("sub_unit", ""), ("sub_food", ""), ("sub_note", ""),
        ("locked", "on"), ("hide_nutrition", "on"), ("landscape_view", "on"),
    ]);
    let (status, _, body) = post_form(&s.app, &uri, &enc(&f)).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    let c = s.db.get_recipe(&s.sc, r.id).await.unwrap().unwrap().content;
    assert_eq!(c.notes, [Note { title: "Tip".into(), text: "Salt late".into() }]);
    assert_eq!(c.extras.get("source").map(String::as_str), Some("gran"));
    assert_eq!(c.extras.len(), 1, "blank rows are skipped");
    assert!(c.ingredients[0].substitutions.is_empty(), "the rows are the whole truth once `features` is sent");
    assert_eq!(c.ingredients[1].substitutions, [Substitution { qty: Some(2.0), unit: Some("tbsp".into()), food: Some("butter".into()), note: "if no oil".into() }]);
    assert_eq!((c.settings.locked, c.settings.hide_nutrition, c.settings.landscape_view, c.settings.public), (true, true, true, false));

    // locked: a post without the box is an unlock; a post that keeps it ticked, or an older form, is refused
    let (status, _, body) = post_form(&s.app, &uri, &enc(&[("name", "Other"), ("ingredients", "2 onions")])).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("recipe is locked"), "{body}");
    let mut keep: Vec<(&str, &str)> = base.to_vec();
    keep.extend([("locked", "on")]);
    assert_eq!(post_form(&s.app, &uri, &enc(&keep)).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, ..) = post_form(&s.app, &uri, &enc(&base)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let c = s.db.get_recipe(&s.sc, r.id).await.unwrap().unwrap().content;
    assert!(!c.settings.locked && c.notes.is_empty() && c.extras.is_empty(), "unticked and unfilled: cleared");
}

#[tokio::test]
async fn the_edit_page_offers_rows_for_what_the_recipe_has() {
    let s = site().await;
    let r = soup(&s).await;
    s.db
        .update_recipe(&s.sc, r.id, RecipeInput { notes: Some(vec![Note { title: "Tip".into(), text: "Salt late".into() }]), extras: Some([("source".to_string(), "gran".to_string())].into()), settings: Some(Settings { locked: false, disable_comments: true, ..Default::default() }), ..mealier::RecipeInput::from(r.content.clone()) })
        .await
        .unwrap();
    let (status, page) = get(&s.app, &format!("/recipes/{}/edit", r.id)).await;
    assert_eq!(status, StatusCode::OK);
    for needle in [
        "name=\"features\"",
        "name=\"note_title\" value=\"Tip\"",
        "Salt late</textarea>",
        "name=\"extra_key\" value=\"source\"",
        "name=\"extra_value\" value=\"gran\"",
        "name=\"sub_line\" value=\"1\"",
        "name=\"sub_food\" value=\"shallots\"",
        "name=\"sub_unit\" value=\"tbsp\"",
        "name=\"disable_comments\" checked",
        "name=\"locked\"",
        "href=\"/recipes/",
        "/link-ingredients",
        "action=\"/recipes/",
    ] {
        assert!(page.contains(needle), "the edit page lacks {needle:?}\n{page}");
    }
    assert!(!page.contains("name=\"locked\" checked"), "{page}");
}

// ---------- linking ingredients to steps ----------

#[tokio::test]
async fn the_link_page_has_a_box_for_every_step_and_line_and_saves_the_ticked_ones() {
    let s = site().await;
    let r = soup(&s).await;
    let (a, b) = (r.content.ingredients[0].uid.clone(), r.content.ingredients[1].uid.clone());
    let uri = format!("/recipes/{}/link-ingredients", r.id);
    let (status, page) = get(&s.app, &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page.matches("name=\"link\"").count(), 6, "3 steps x 2 lines: {page}");
    assert!(page.contains(&format!("value=\"0:{a}\" checked")) && page.contains(&format!("value=\"1:{b}\" checked")), "{page}");
    assert!(!page.contains(&format!("value=\"0:{b}\" checked")), "{page}");

    let (status, loc, body) = post_form(&s.app, &uri, &enc(&[("link", &format!("2:{b}")), ("link", &format!("2:{a}")), ("link", &format!("0:{a}"))])).await;
    assert_eq!((status, loc.as_deref()), (StatusCode::SEE_OTHER, Some(format!("/recipes/{}", r.id).as_str())), "{body}");
    let c = s.db.get_recipe(&s.sc, r.id).await.unwrap().unwrap().content;
    assert_eq!(c.step_details.iter().map(|d| d.ingredient_uids.clone()).collect::<Vec<_>>(), [vec![a.clone()], vec![], vec![b.clone(), a.clone()]]);
    assert_eq!(c.step_details[0].title, "Prep", "titles are left alone");

    for bad in [format!("9:{a}"), "0:nope".to_string(), "x".to_string()] {
        let (status, ..) = post_form(&s.app, &uri, &enc(&[("link", &bad)])).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
}

// ---------- the recipe page ----------

#[tokio::test]
async fn the_recipe_page_shows_sections_alternatives_notes_extras_assets_and_the_lock() {
    let s = site().await;
    let r = soup(&s).await;
    let onion = s.db.add_food(&s.sc, "onion").await.unwrap();
    let leek = s.db.add_food(&s.sc, "leek").await.unwrap();
    s.db.add_food_substitution(&s.sc, onion, SubstitutionInput { food_id: leek, note: "milder".into() }).await.unwrap();
    let mut input = RecipeInput::from(r.content.clone());
    input.ingredients[0].food_id = Some(onion);
    input.notes = Some(vec![Note { title: "Tip".into(), text: "Salt late".into() }]);
    input.extras = Some([("source".to_string(), "gran".to_string())].into());
    input.nutrition = Some(mealier::Nutrition { calories: Some(250.0), ..Default::default() });
    s.db.update_recipe(&s.sc, r.id, input).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let asset = s.db.add_recipe_asset(&s.sc, r.id, dir.path(), "menu.pdf", "The menu", "mdi-file-pdf-box", b"%PDF").await.unwrap();

    let (status, page) = get(&s.app, &format!("/recipes/{}", r.id)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("<h3>Base</h3>"), "section heading: {page}");
    assert!(page.contains("or 1.5 tbsp shallots (milder)"), "the line's own alternative: {page}");
    assert!(page.contains("or leek (milder)"), "the food's alternative: {page}");
    assert!(page.contains("Prep") && page.contains("Chop"), "step title and text");
    assert!(page.contains("Tip") && page.contains("Salt late"), "notes: {page}");
    assert!(page.contains("source") && page.contains("gran"), "extras: {page}");
    assert!(!page.contains("name=\"extra_key\""), "read-only on the page");
    assert!(page.contains(&format!("/api/assets/{}?recipe={}", asset.hash, r.id)) && page.contains("The menu"), "asset link: {page}");
    assert!(page.contains("250"), "nutrition shown: {page}");
    assert!(!page.contains("locked"), "{page}");

    let mut input = RecipeInput::from(s.db.get_recipe(&s.sc, r.id).await.unwrap().unwrap().content);
    input.settings = Some(Settings { locked: true, hide_nutrition: true, ..Default::default() });
    s.db.update_recipe(&s.sc, r.id, input).await.unwrap();
    let (_, page) = get(&s.app, &format!("/recipes/{}", r.id)).await;
    assert!(!page.contains("250"), "hide nutrition: {page}");
    assert!(page.contains("locked"), "{page}");
}

// ---------- cook mode ----------

#[tokio::test]
async fn cook_mode_shows_each_steps_linked_ingredients_under_it_scaled_and_from_a_batch_snapshot() {
    let s = site().await;
    let onion = s.db.add_food(&s.sc, "onion").await.unwrap();
    let oil = s.db.add_food(&s.sc, "oil").await.unwrap();
    let ing = |food, qty| IngredientInput { qty_min: Some(qty), qty_max: Some(qty), food_id: Some(food), ..Default::default() };
    let r = s.db.create_recipe(&s.sc, RecipeInput { name: "Soup".into(), ingredients: vec![ing(onion, 2.0), ing(oil, 1.0)], steps: vec!["Chop".into(), "Fry".into()], ..Default::default() }).await.unwrap();
    let (a, b) = (r.content.ingredients[0].uid.clone(), r.content.ingredients[1].uid.clone());
    let mut input = RecipeInput::from(r.content.clone());
    input.step_details = vec![StepDetail { title: "Prep".into(), ingredient_uids: vec![a.clone()] }, StepDetail { title: String::new(), ingredient_uids: vec![a.clone(), b.clone()] }];
    s.db.update_recipe(&s.sc, r.id, input.clone()).await.unwrap();

    let step_block = |page: &str, text: &str| -> String {
        let from = page.find(&format!("<span>{text}</span>")).unwrap_or_else(|| panic!("no step {text:?} in {page}"));
        page[from..].split("</li>").next().unwrap().to_string()
    };
    let (status, page) = get(&s.app, &format!("/recipes/{}/cook?scale=2", r.id)).await;
    assert_eq!(status, StatusCode::OK);
    let chop = step_block(&page, "Chop");
    assert!(chop.contains("4 onion") && !chop.contains("oil"), "{chop}");
    let fry = step_block(&page, "Fry");
    assert!(fry.contains("4 onion") && fry.contains("2 oil"), "{fry}");
    assert!(page.contains("Prep"), "the step title: {page}");

    // a batch cooks from its own snapshot, with its own amounts
    let batch = s.db.start_batch(&s.sc, r.id, 3.0).await.unwrap().batch.id;
    input.step_details = vec![StepDetail::default(), StepDetail::default()];
    s.db.update_recipe(&s.sc, r.id, input).await.unwrap();
    let (_, page) = get(&s.app, &format!("/recipes/{}/cook?batch={batch}", r.id)).await;
    assert!(step_block(&page, "Chop").contains("6 onion"), "{page}");
    assert!(step_block(&page, "Fry").contains("3 oil"), "{page}");
    let (_, live) = get(&s.app, &format!("/recipes/{}/cook", r.id)).await;
    assert!(!step_block(&live, "Chop").contains("onion"), "the live recipe has no links now: {live}");
}

// ---------- attachments ----------

#[tokio::test]
async fn attachments_are_added_and_removed_from_the_page() {
    let s = site().await;
    let r = s.db.create_recipe(&s.sc, RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap();
    let body = b"--XX\r\nContent-Disposition: form-data; name=\"name\"\r\n\r\nThe menu\r\n--XX\r\nContent-Disposition: form-data; name=\"file\"; filename=\"menu.pdf\"\r\nContent-Type: application/pdf\r\n\r\n%PDF-1.4\r\n--XX--\r\n".to_vec();
    let uri = format!("/recipes/{}/assets", r.id);
    let (status, loc, text) = send(&s.app, "POST", &uri, Some("multipart/form-data; boundary=XX"), body.clone()).await;
    assert_eq!((status, loc.as_deref()), (StatusCode::SEE_OTHER, Some(format!("/recipes/{}", r.id).as_str())), "{text}");
    let assets = s.db.recipe_assets(&s.sc, r.id).await.unwrap();
    assert_eq!((assets.len(), assets[0].name.as_str(), assets[0].file_name.as_str()), (1, "The menu", "menu.pdf"));
    let (_, edit) = get(&s.app, &format!("/recipes/{}/edit", r.id)).await;
    assert!(edit.contains(&format!("action=\"/recipes/{}/assets/{}/delete\"", r.id, assets[0].hash)) && edit.contains("name=\"file\""), "{edit}");

    let (status, ..) = post_form(&s.app, &format!("/recipes/{}/assets/{}/delete", r.id, assets[0].hash), "").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(s.db.recipe_assets(&s.sc, r.id).await.unwrap().is_empty());

    let mut input = RecipeInput::from(s.db.get_recipe(&s.sc, r.id).await.unwrap().unwrap().content);
    input.settings = Some(Settings { locked: true, ..Default::default() });
    s.db.update_recipe(&s.sc, r.id, input).await.unwrap();
    let (status, _, text) = send(&s.app, "POST", &uri, Some("multipart/form-data; boundary=XX"), body).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(text.contains("recipe is locked"), "{text}");
}

// ---------- shopping lists ----------

#[tokio::test]
async fn a_shopping_list_page_shows_its_extras_and_its_items_read_only() {
    let s = site().await;
    let list = s.db.create_list(&s.sc, "Weekly").await.unwrap().id;
    s.db.set_list_extras(&s.sc, list, [("store".to_string(), "co-op".to_string())].into()).await.unwrap();
    s.db.add_item(&s.sc, list, mealier::ItemInput { note: "milk".into(), extras: Some([("brand".to_string(), "Hilltop".to_string())].into()), ..Default::default() }).await.unwrap();
    let (status, page) = get(&s.app, &format!("/lists/{list}")).await;
    assert_eq!(status, StatusCode::OK);
    for needle in ["store", "co-op", "brand", "Hilltop"] {
        assert!(page.contains(needle), "{needle}: {page}");
    }
    assert!(!page.contains("name=\"extra_key\""), "read-only: {page}");
}

// ---------- the review screen keeps what the lines carry ----------

#[tokio::test]
async fn saving_the_ingredient_review_keeps_ids_sections_and_substitutions_and_prunes_links_of_dropped_rows() {
    let s = site().await;
    let r = soup(&s).await;
    let (a, b) = (r.content.ingredients[0].uid.clone(), r.content.ingredients[1].uid.clone());
    let uri = format!("/recipes/{}/ingredients/review", r.id);
    let both = enc(&[("original_text", "2 onions"), ("note", "diced"), ("original_text", "1 tbsp oil"), ("note", "")]);
    let (status, ..) = post_form(&s.app, &uri, &both).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let c = s.db.get_recipe(&s.sc, r.id).await.unwrap().unwrap().content;
    assert_eq!(c.ingredients.iter().map(|i| i.uid.clone()).collect::<Vec<_>>(), [a.clone(), b.clone()]);
    assert_eq!((c.ingredients[0].note.as_str(), c.ingredients[0].section.as_str(), c.ingredients[0].substitutions.len()), ("diced", "Base", 1));
    assert_eq!(c.step_details[1].ingredient_uids, [a.clone(), b]);

    let (status, ..) = post_form(&s.app, &uri, &enc(&[("original_text", "2 onions")])).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let c = s.db.get_recipe(&s.sc, r.id).await.unwrap().unwrap().content;
    assert_eq!(c.step_details[1].ingredient_uids, [a], "the dropped line's link went with it");
}
