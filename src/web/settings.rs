//! Settings pages: cook theme, webhooks and the LLM (the last two admin only; each handler takes `Admin`,
//! and the guard middleware refuses non-admins under these paths as well). Households have their own module.
use askama::Template;
use askama_web::WebTemplate;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::Redirect;
use axum::routing::{get, post};
use serde::Deserialize;

use super::Svc;
use crate::api::AppState;
use crate::{Db, Error, Result, Scope};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/settings/cook-theme", post(set_theme))
        .route("/settings/webhooks", get(webhooks_page).post(webhook_create))
        .route("/settings/webhooks/{id}/delete", post(webhook_delete))
        .route("/settings/webhooks/{id}/toggle", post(webhook_toggle))
        .route("/settings/webhooks/{id}/test", post(webhook_test))
        .route("/settings/labels", get(labels_page).post(label_create))
        .route("/settings/labels/{id}", post(label_update))
        .route("/settings/labels/{id}/delete", post(label_delete))
        .route("/pantry", get(pantry_page))
        .route("/pantry/foods/{id}", post(pantry_food))
        .route("/pantry/tools/{id}", post(pantry_tool))
        .route("/settings/actions", get(actions_page).post(action_create))
        .route("/settings/actions/{id}/delete", post(action_delete))
        .route("/settings/llm", get(llm_page))
        .route("/settings/llm/test", post(llm_test))
}

#[derive(Deserialize)]
struct ThemeForm {
    theme: String,
    back: String,
}

async fn set_theme(crate::CurrentUser { user, .. }: crate::CurrentUser, State(db): State<Db>, axum::Form(f): axum::Form<ThemeForm>) -> Result<Redirect> {
    db.set_cook_theme(user.id, &f.theme).await?;
    // Same-site paths only, so this cannot bounce a browser elsewhere.
    let back = if f.back.starts_with('/') && !f.back.starts_with("//") && !f.back.contains('\\') { f.back } else { "/".into() };
    Ok(Redirect::to(&back))
}

#[derive(Template, WebTemplate)]
#[template(path = "webhooks.html")]
struct WebhooksPage {
    hooks: Vec<crate::Webhook>,
    msg: String,
}

#[derive(Deserialize)]
struct MsgQuery {
    msg: Option<String>,
}

async fn webhooks_page(_: crate::Admin, State(db): State<Db>, Query(q): Query<MsgQuery>) -> Result<WebhooksPage> {
    Ok(WebhooksPage { hooks: db.list_webhooks().await?, msg: q.msg.unwrap_or_default() })
}

#[derive(Deserialize)]
struct WebhookForm {
    name: String,
    kind: String,
    url: String,
    #[serde(default)]
    ntfy_tags: String,
    ev_import_failed: Option<String>,
    ev_batch_logged: Option<String>,
}

async fn webhook_create(_: crate::Admin, State(db): State<Db>, axum::Form(f): axum::Form<WebhookForm>) -> Result<Redirect> {
    let events = [("import_failed", f.ev_import_failed), ("batch_logged", f.ev_batch_logged)].into_iter().filter(|(_, on)| on.is_some()).map(|(e, _)| e.to_string()).collect();
    db.create_webhook(crate::WebhookInput { name: f.name, kind: f.kind, url: f.url, events, ntfy_tags: f.ntfy_tags, enabled: true }).await?;
    Ok(Redirect::to("/settings/webhooks"))
}

async fn webhook_delete(_: crate::Admin, State(db): State<Db>, Path(id): Path<i64>) -> Result<Redirect> {
    db.delete_webhook(id).await?;
    Ok(Redirect::to("/settings/webhooks"))
}

async fn webhook_toggle(_: crate::Admin, State(db): State<Db>, Path(id): Path<i64>) -> Result<Redirect> {
    let w = db.get_webhook(id).await?;
    db.update_webhook(id, crate::WebhookInput { name: w.name, kind: w.kind, url: w.url, events: w.events, ntfy_tags: w.ntfy_tags, enabled: !w.enabled }).await?;
    Ok(Redirect::to("/settings/webhooks"))
}

async fn webhook_test(_: crate::Admin, State(db): State<Db>, Path(id): Path<i64>) -> Result<Redirect> {
    let msg = match db.send_test(id).await {
        Ok(s) => format!("Test sent; the receiver answered {s}."),
        Err(Error::NotFound(n)) => return Err(Error::NotFound(n)),
        Err(e) => e.to_string(),
    };
    Ok(Redirect::to(&format!("/settings/webhooks?{}", serde_urlencoded::to_string([("msg", msg)]).unwrap_or_default())))
}

// Labels (group-wide; any signed-in user).

#[derive(Template, WebTemplate)]
#[template(path = "labels.html")]
struct LabelsPage {
    labels: Vec<crate::Label>,
}

async fn labels_page(sc: Scope, State(db): State<Db>) -> Result<LabelsPage> {
    Ok(LabelsPage { labels: db.labels(&sc).await? })
}

async fn label_create(sc: Scope, State(db): State<Db>, axum::Form(f): axum::Form<crate::LabelInput>) -> Result<Redirect> {
    db.add_label(&sc, f).await?;
    Ok(Redirect::to("/settings/labels"))
}

async fn label_update(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, axum::Form(f): axum::Form<crate::LabelInput>) -> Result<Redirect> {
    db.update_label(&sc, id, f).await?;
    Ok(Redirect::to("/settings/labels"))
}

async fn label_delete(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<Redirect> {
    db.delete_label(&sc, id).await?;
    Ok(Redirect::to("/settings/labels"))
}

// LLM settings.

#[derive(Template, WebTemplate)]
#[template(path = "llm.html")]
struct LlmPage {
    enabled: bool,
    provider: String,
    model: String,
    msg: String,
}

fn llm_view(svc: &crate::ImportService, msg: String) -> LlmPage {
    let llm = svc.llm();
    LlmPage {
        enabled: llm.is_some(),
        provider: llm.map(|l| serde_json::to_value(l.provider()).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()).unwrap_or_default(),
        model: llm.map(|l| l.model().to_string()).unwrap_or_default(),
        msg,
    }
}

async fn llm_page(_: crate::Admin, axum::Extension(svc): Svc) -> LlmPage {
    llm_view(&svc, String::new())
}

async fn llm_test(_: crate::Admin, axum::Extension(svc): Svc) -> LlmPage {
    let msg = match svc.llm() {
        None => "No LLM is configured.".to_string(),
        Some(l) => match l.test().await {
            Ok(()) => "Test passed.".to_string(),
            Err(e) => format!("Test failed: {e}"),
        },
    };
    llm_view(&svc, msg)
}

#[derive(Template, WebTemplate)]
#[template(path = "pantry.html")]
struct PantryPage {
    pantry: crate::Pantry,
}

async fn pantry_page(sc: Scope, State(db): State<Db>) -> Result<PantryPage> {
    Ok(PantryPage { pantry: db.pantry(&sc).await? })
}

#[derive(Deserialize)]
struct HaveForm {
    #[serde(default)]
    have: String,
}

async fn pantry_food(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, axum::Form(f): axum::Form<HaveForm>) -> Result<Redirect> {
    db.set_food_on_hand(&sc, id, f.have == "1").await?;
    Ok(Redirect::to("/pantry"))
}

async fn pantry_tool(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, axum::Form(f): axum::Form<HaveForm>) -> Result<Redirect> {
    db.set_tool_owned(&sc, id, f.have == "1").await?;
    Ok(Redirect::to("/pantry"))
}

#[derive(Template, WebTemplate)]
#[template(path = "actions.html")]
struct ActionsPage {
    actions: Vec<crate::RecipeAction>,
    admin: bool,
}

async fn actions_page(sc: Scope, State(db): State<Db>) -> Result<ActionsPage> {
    Ok(ActionsPage { actions: db.actions(&sc).await?, admin: sc.admin })
}

#[derive(Deserialize)]
struct ActionForm {
    title: String,
    kind: String,
    url: String,
}

async fn action_create(sc: Scope, State(db): State<Db>, axum::Form(f): axum::Form<ActionForm>) -> Result<Redirect> {
    db.create_action(&sc, crate::ActionInput { title: f.title, kind: f.kind, url: f.url }).await?;
    Ok(Redirect::to("/settings/actions"))
}

async fn action_delete(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<Redirect> {
    db.delete_action(&sc, id).await?;
    Ok(Redirect::to("/settings/actions"))
}
