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
        .route("/settings/webhooks/{id}/edit", get(webhook_edit_page).post(webhook_edit))
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

/// One choice of a `<select>`.
struct Choice {
    value: i64,
    label: String,
    selected: bool,
}

/// One event checkbox.
struct EventBox {
    name: &'static str,
    on: bool,
}

struct HookRow {
    h: crate::Webhook,
    shown_url: String,
    /// "" for every group.
    group: String,
    household: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "webhooks.html")]
struct WebhooksPage {
    hooks: Vec<HookRow>,
    msg: String,
    groups: Vec<Choice>,
    households: Vec<Choice>,
    events: Vec<EventBox>,
}

#[derive(Deserialize)]
struct MsgQuery {
    msg: Option<String>,
}

/// Group and household choices, `selected` marking the given ids.
async fn choices(db: &Db, group: Option<i64>, household: Option<i64>) -> Result<(Vec<Choice>, Vec<Choice>)> {
    let groups = db.groups().await?;
    let name = |id: i64| groups.iter().find(|g| g.id == id).map(|g| g.name.clone()).unwrap_or_default();
    let households = db.households().await?.into_iter().map(|h| Choice { value: h.id, label: format!("{} / {}", name(h.group_id), h.name), selected: Some(h.id) == household }).collect();
    Ok((groups.iter().map(|g| Choice { value: g.id, label: g.name.clone(), selected: Some(g.id) == group }).collect(), households))
}

async fn webhooks_page(_: crate::Admin, State(db): State<Db>, Query(q): Query<MsgQuery>) -> Result<WebhooksPage> {
    let (groups, households) = choices(&db, None, None).await?;
    let label = |cs: &[Choice], id: Option<i64>| id.and_then(|i| cs.iter().find(|c| c.value == i)).map(|c| c.label.clone()).unwrap_or_default();
    let hooks = db.list_webhooks().await?.into_iter().map(|h| HookRow { shown_url: h.masked_url(), group: label(&groups, h.group_id), household: label(&households, h.household_id), h }).collect();
    let events = crate::EVENTS.iter().map(|e| EventBox { name: e, on: matches!(*e, "import_failed" | "batch_logged") }).collect();
    Ok(WebhooksPage { hooks, msg: q.msg.unwrap_or_default(), groups, households, events })
}

/// The first value of `key` in a form.
fn field<'a>(form: &'a [(String, String)], key: &str) -> Option<&'a str> {
    form.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// The webhook a form describes. `current` is the webhook being edited: its kind stays, and a blank URL keeps its URL.
/// A scheduled webhook is enabled only if ticked (a new one starts disabled); other new ones are enabled.
fn webhook_from_form(body: &[u8], current: Option<&crate::Webhook>) -> Result<crate::WebhookInput> {
    let form = super::pairs(body)?;
    let text = |k: &str| field(&form, k).unwrap_or_default().trim().to_string();
    let kind = current.map_or_else(|| text("kind"), |w| w.kind.clone());
    let scheduled = kind == "scheduled";
    let url = match (text("url"), current) {
        (u, Some(w)) if u.is_empty() => w.url.clone(),
        (u, _) => u,
    };
    let ticked = field(&form, "enabled").is_some();
    Ok(crate::WebhookInput {
        name: text("name"),
        kind,
        url,
        events: if scheduled { vec![] } else { crate::EVENTS.iter().filter(|e| field(&form, &format!("ev_{e}")).is_some()).map(|e| e.to_string()).collect() },
        ntfy_tags: text("ntfy_tags"),
        enabled: ticked || (current.is_none() && !scheduled),
        group_id: if scheduled { None } else { super::id_of(&text("group_id"))? },
        household_id: if scheduled { super::id_of(&text("household_id"))? } else { None },
        time_of_day: Some(text("time_of_day")).filter(|t| scheduled && !t.is_empty()),
    })
}

async fn webhook_create(_: crate::Admin, State(db): State<Db>, body: axum::body::Bytes) -> Result<Redirect> {
    db.create_webhook(webhook_from_form(&body, None)?).await?;
    Ok(Redirect::to("/settings/webhooks"))
}

#[derive(Template, WebTemplate)]
#[template(path = "webhook_edit.html")]
struct WebhookEditPage {
    h: crate::Webhook,
    shown_url: String,
    groups: Vec<Choice>,
    households: Vec<Choice>,
    events: Vec<EventBox>,
}

async fn webhook_edit_page(_: crate::Admin, State(db): State<Db>, Path(id): Path<i64>) -> Result<WebhookEditPage> {
    let h = db.get_webhook(id).await?;
    let (groups, households) = choices(&db, h.group_id, h.household_id).await?;
    let events = crate::EVENTS.iter().map(|e| EventBox { name: e, on: h.events.iter().any(|x| x == e) }).collect();
    Ok(WebhookEditPage { shown_url: h.masked_url(), h, groups, households, events })
}

async fn webhook_edit(_: crate::Admin, State(db): State<Db>, Path(id): Path<i64>, body: axum::body::Bytes) -> Result<Redirect> {
    let current = db.get_webhook(id).await?;
    db.update_webhook(id, webhook_from_form(&body, Some(&current))?).await?;
    Ok(Redirect::to("/settings/webhooks"))
}

async fn webhook_delete(_: crate::Admin, State(db): State<Db>, Path(id): Path<i64>) -> Result<Redirect> {
    db.delete_webhook(id).await?;
    Ok(Redirect::to("/settings/webhooks"))
}

async fn webhook_toggle(_: crate::Admin, State(db): State<Db>, Path(id): Path<i64>) -> Result<Redirect> {
    let w = db.get_webhook(id).await?;
    db.update_webhook(id, crate::WebhookInput { name: w.name, kind: w.kind, url: w.url, events: w.events, ntfy_tags: w.ntfy_tags, enabled: !w.enabled, group_id: w.group_id, household_id: w.household_id, time_of_day: w.time_of_day }).await?;
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
