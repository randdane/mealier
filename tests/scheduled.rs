//! Build-order step 15, part 2: scheduled webhooks, `Db::run_scheduled` and `mealier run-scheduled`.
mod common;
use std::time::Duration;

use mealier::{Clock, Db, EntryInput, Error, Notifier, RecipeInput, RunReport, Scope, WebhookInput};
use serde_json::{Value, json};
use sqlx::Row;

fn clock(date: &str, hhmm: &str) -> Clock {
    let (h, m) = hhmm.split_once(':').unwrap();
    Clock { date: date.into(), minutes: h.parse::<u32>().unwrap() * 60 + m.parse::<u32>().unwrap() }
}

fn scheduled(url: &str, at: &str, household_id: i64, enabled: bool) -> WebhookInput {
    WebhookInput { name: "plan".into(), kind: "scheduled".into(), url: url.into(), events: vec![], ntfy_tags: String::new(), enabled, group_id: None, household_id: Some(household_id), time_of_day: Some(at.into()) }
}

async fn db_with(base: Option<&str>) -> Db {
    Db::open_memory().await.unwrap().with_notifier(Notifier::new(Duration::from_millis(500), base.map(String::from)))
}

async fn plan(db: &Db, sc: &Scope, date: &str, slot: &str, recipe: Option<i64>, title: &str) {
    db.add_entry(sc, EntryInput { date: date.into(), slot: slot.into(), recipe_id: recipe, title: title.into(), text: "note".into(), ..Default::default() }).await.unwrap();
}

async fn runs(db: &Db, id: i64) -> Vec<(String, String, String, i64)> {
    sqlx::query("SELECT run_date, status, error, attempts FROM webhook_runs WHERE webhook_id = ? ORDER BY run_date").bind(id).fetch_all(db.pool()).await.unwrap().iter().map(|r| (r.get(0), r.get(1), r.get(2), r.get(3))).collect()
}

// ---- the clock ----

#[test]
fn clock_reads_unix_time_in_utc() {
    assert_eq!(Clock::from_unix(0, false), Clock { date: "1970-01-01".into(), minutes: 0 });
    assert_eq!(Clock::from_unix(1_700_000_000, false), Clock { date: "2023-11-14".into(), minutes: 22 * 60 + 13 });
    assert_eq!(Clock::from_unix(1_709_164_799, false), Clock { date: "2024-02-28".into(), minutes: 23 * 60 + 59 }, "leap year");
}

#[test]
fn clock_in_local_time_is_within_a_day_of_utc() {
    let (u, l) = (Clock::from_unix(1_700_000_000, false), Clock::from_unix(1_700_000_000, true));
    assert!(l.minutes < 1440);
    let day = |c: &Clock| c.date[8..].parse::<i32>().unwrap() * 1440 + c.minutes as i32;
    assert!((day(&u) - day(&l)).abs() <= 14 * 60, "{u:?} {l:?}");
    let now = Clock::now();
    assert!(now.date.len() == 10 && now.minutes < 1440, "{now:?}");
}

// ---- the webhook itself ----

#[tokio::test]
async fn a_scheduled_webhook_round_trips_and_takes_its_group_from_the_household() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let w = db.create_webhook(scheduled("http://127.0.0.1:9/x", "07:30", t.a.household_id, false)).await.unwrap();
    assert_eq!((w.kind.as_str(), w.time_of_day.as_deref(), w.household_id, w.group_id, w.enabled), ("scheduled", Some("07:30"), Some(t.a.household_id), Some(t.a.group_id), false));
    assert!(w.events.is_empty());
    assert_eq!(db.get_webhook(w.id).await.unwrap().time_of_day.as_deref(), Some("07:30"));
    let u = db.update_webhook(w.id, scheduled("http://127.0.0.1:9/y", "18:05", t.c.household_id, true)).await.unwrap();
    assert_eq!((u.time_of_day.as_deref(), u.group_id, u.enabled), (Some("18:05"), Some(t.c.group_id), true));
}

#[tokio::test]
async fn a_scheduled_webhook_needs_a_real_time_and_household() {
    let db = Db::open_memory().await.unwrap();
    let h = common::household(&db).await;
    for bad in ["25:00", "7:30", "07:60", "07:3", "0730", "", "07:30:00", "ab:cd"] {
        assert!(matches!(db.create_webhook(scheduled("http://127.0.0.1:9/x", bad, h, true)).await, Err(Error::Validation(_))), "{bad:?}");
    }
    let mut no_time = scheduled("http://127.0.0.1:9/x", "07:30", h, true);
    no_time.time_of_day = None;
    assert!(matches!(db.create_webhook(no_time).await, Err(Error::Validation(_))));
    let mut no_household = scheduled("http://127.0.0.1:9/x", "07:30", h, true);
    no_household.household_id = None;
    assert!(matches!(db.create_webhook(no_household).await, Err(Error::Validation(_))));
    assert!(matches!(db.create_webhook(scheduled("http://127.0.0.1:9/x", "07:30", h + 99, true)).await, Err(Error::Validation(_))));
    let mut wrong_group = scheduled("http://127.0.0.1:9/x", "07:30", h, true);
    wrong_group.group_id = Some(h + 99);
    assert!(matches!(db.create_webhook(wrong_group).await, Err(Error::Validation(_))));
    // a time on an ordinary webhook is a mistake, not ignored
    let mut json = scheduled("http://127.0.0.1:9/x", "07:30", h, true);
    json.kind = "json".into();
    json.events = vec!["recipe_created".into()];
    assert!(matches!(db.create_webhook(json).await, Err(Error::Validation(_))));
    assert!(db.list_webhooks().await.unwrap().is_empty());
}

// ---- running them ----

#[tokio::test]
async fn it_sends_todays_plan_once_the_time_has_passed_and_never_twice() {
    let (url, h) = common::receiver(200).await;
    let db = db_with(Some("https://meals.test")).await;
    let sc = common::scope(&db).await;
    let soup = db.create_recipe(&sc, RecipeInput { name: "Crème Soup!".into(), ..Default::default() }).await.unwrap();
    plan(&db, &sc, "2026-11-06", "dinner", Some(soup.id), "").await;
    plan(&db, &sc, "2026-11-06", "breakfast", None, "Toast").await;
    plan(&db, &sc, "2026-11-07", "dinner", Some(soup.id), "").await; // tomorrow
    let w = db.create_webhook(scheduled(&format!("{url}/plan"), "07:30", sc.household_id, true)).await.unwrap();

    assert_eq!(db.run_scheduled(&clock("2026-11-06", "07:29")).await.unwrap(), RunReport { not_due: 1, ..Default::default() });
    assert!(common::hits(&h, 1).await.is_empty());

    assert_eq!(db.run_scheduled(&clock("2026-11-06", "07:30")).await.unwrap(), RunReport { sent: 1, ..Default::default() });
    let got = common::hits(&h, 1).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].path, "/plan");
    assert_eq!(got[0].headers["content-type"], "application/json");
    let v: Value = serde_json::from_str(&got[0].body).unwrap();
    assert_eq!(v["date"], "2026-11-06");
    assert_eq!((v["household_id"].clone(), v["group_id"].clone()), (json!(sc.household_id), json!(sc.group_id)));
    assert_eq!(
        v["entries"],
        json!([
            {"date": "2026-11-06", "slot": "breakfast", "title": "Toast", "text": "note", "recipe": null},
            {"date": "2026-11-06", "slot": "dinner", "title": "", "text": "note", "recipe": {"id": soup.id, "name": "Crème Soup!", "slug": "cr-me-soup", "url": format!("https://meals.test/recipes/{}", soup.id)}},
        ]),
        "breakfast before dinner, today only"
    );

    // later the same day, and an hour on: not again
    assert_eq!(db.run_scheduled(&clock("2026-11-06", "08:30")).await.unwrap(), RunReport { already_sent: 1, ..Default::default() });
    assert_eq!(db.run_scheduled(&clock("2026-11-06", "23:59")).await.unwrap().sent, 0);
    assert_eq!(common::hits(&h, 2).await.len(), 1);
    assert_eq!(runs(&db, w.id).await, [("2026-11-06".into(), "sent".into(), String::new(), 1)]);

    // tomorrow is a new day
    assert_eq!(db.run_scheduled(&clock("2026-11-07", "07:30")).await.unwrap().sent, 1);
    assert_eq!(common::hits(&h, 2).await.len(), 2);
}

#[tokio::test]
async fn it_catches_up_when_run_late_and_skips_disabled_ones() {
    let (url, h) = common::receiver(200).await;
    let db = db_with(None).await;
    let sc = common::scope(&db).await;
    plan(&db, &sc, "2026-11-06", "lunch", None, "Leftovers").await;
    db.create_webhook(scheduled(&format!("{url}/off"), "01:00", sc.household_id, false)).await.unwrap();
    db.create_webhook(scheduled(&format!("{url}/late"), "06:00", sc.household_id, true)).await.unwrap();
    db.create_webhook(scheduled(&format!("{url}/early"), "00:00", sc.household_id, true)).await.unwrap();
    db.create_webhook(scheduled(&format!("{url}/never"), "23:59", sc.household_id, true)).await.unwrap();

    let r = db.run_scheduled(&clock("2026-11-06", "12:00")).await.unwrap();
    assert_eq!(r, RunReport { sent: 2, not_due: 1, ..Default::default() });
    let mut paths: Vec<String> = common::hits(&h, 2).await.into_iter().map(|g| g.path).collect();
    paths.sort();
    assert_eq!(paths, ["/early", "/late"]);
}

#[tokio::test]
async fn each_webhook_gets_its_own_households_plan_and_an_empty_plan_sends_nothing() {
    let (url, h) = common::receiver(200).await;
    let db = db_with(None).await;
    let t = common::tenants(&db).await;
    plan(&db, &t.a, "2026-11-06", "dinner", None, "Alpha dinner").await;
    plan(&db, &t.b, "2026-11-06", "dinner", None, "Beta dinner").await;
    db.create_webhook(scheduled(&format!("{url}/alpha"), "06:00", t.a.household_id, true)).await.unwrap();
    db.create_webhook(scheduled(&format!("{url}/beta"), "06:00", t.b.household_id, true)).await.unwrap();
    let gamma = db.create_webhook(scheduled(&format!("{url}/gamma"), "06:00", t.c.household_id, true)).await.unwrap();

    let r = db.run_scheduled(&clock("2026-11-06", "06:00")).await.unwrap();
    assert_eq!(r, RunReport { sent: 2, empty: 1, ..Default::default() });
    let got = common::hits(&h, 2).await;
    assert_eq!(got.len(), 2);
    for g in &got {
        let v: Value = serde_json::from_str(&g.body).unwrap();
        let want = if g.path == "/alpha" { "Alpha dinner" } else { "Beta dinner" };
        assert_eq!(v["entries"].as_array().unwrap().len(), 1, "{}", g.body);
        assert_eq!(v["entries"][0]["title"], want, "{}", g.path);
    }
    assert!(runs(&db, gamma.id).await.is_empty(), "nothing sent, so it is still due if something is planned later today");
    plan(&db, &t.c, "2026-11-06", "dinner", None, "Gamma dinner").await;
    assert_eq!(db.run_scheduled(&clock("2026-11-06", "09:00")).await.unwrap().sent, 1);
}

#[tokio::test]
async fn a_failure_is_recorded_and_retried_on_the_next_run_that_day() {
    let (url, h) = common::receiver(500).await;
    let db = db_with(None).await;
    let sc = common::scope(&db).await;
    plan(&db, &sc, "2026-11-06", "lunch", None, "Soup").await;
    let w = db.create_webhook(scheduled(&format!("{url}/x"), "06:00", sc.household_id, true)).await.unwrap();

    assert_eq!(db.run_scheduled(&clock("2026-11-06", "06:00")).await.unwrap(), RunReport { failed: 1, ..Default::default() });
    let rows = runs(&db, w.id).await;
    assert_eq!((rows[0].1.as_str(), rows[0].3), ("failed", 1));
    assert!(rows[0].2.contains("500"), "{rows:?}");

    assert_eq!(db.run_scheduled(&clock("2026-11-06", "06:10")).await.unwrap().failed, 1, "tried again");
    assert_eq!(runs(&db, w.id).await[0].3, 2);
    assert_eq!(common::hits(&h, 2).await.len(), 2);

    // the receiver recovers: point the webhook at a good one; the day then completes and stays done
    let (good, h2) = common::receiver(200).await;
    db.update_webhook(w.id, scheduled(&format!("{good}/x"), "06:00", sc.household_id, true)).await.unwrap();
    assert_eq!(db.run_scheduled(&clock("2026-11-06", "06:20")).await.unwrap().sent, 1);
    assert_eq!(runs(&db, w.id).await, [("2026-11-06".into(), "sent".into(), String::new(), 3)]);
    assert_eq!(db.run_scheduled(&clock("2026-11-06", "06:30")).await.unwrap().already_sent, 1);
    assert_eq!(common::hits(&h2, 2).await.len(), 1);
}

#[tokio::test]
async fn an_unreachable_receiver_is_recorded_without_its_url() {
    let db = db_with(None).await;
    let sc = common::scope(&db).await;
    plan(&db, &sc, "2026-11-06", "lunch", None, "Soup").await;
    let w = db.create_webhook(scheduled("http://127.0.0.1:1/hook-secret-token", "06:00", sc.household_id, true)).await.unwrap();
    assert_eq!(db.run_scheduled(&clock("2026-11-06", "06:00")).await.unwrap().failed, 1);
    let rows = runs(&db, w.id).await;
    assert_eq!(rows[0].1, "failed");
    assert!(!rows[0].2.is_empty() && !rows[0].2.contains("hook-secret-token") && !rows[0].2.contains("127.0.0.1"), "{rows:?}");
}

#[tokio::test]
async fn deleting_the_webhook_deletes_its_runs() {
    let (url, _h) = common::receiver(200).await;
    let db = db_with(None).await;
    let sc = common::scope(&db).await;
    plan(&db, &sc, "2026-11-06", "lunch", None, "Soup").await;
    let w = db.create_webhook(scheduled(&format!("{url}/x"), "06:00", sc.household_id, true)).await.unwrap();
    db.run_scheduled(&clock("2026-11-06", "06:00")).await.unwrap();
    db.delete_webhook(w.id).await.unwrap();
    assert!(runs(&db, w.id).await.is_empty());
}

#[tokio::test]
async fn scheduled_webhooks_never_receive_events() {
    let (url, h) = common::receiver(200).await;
    let db = db_with(None).await;
    let sc = common::scope(&db).await;
    db.create_webhook(scheduled(&format!("{url}/x"), "06:00", sc.household_id, true)).await.unwrap();
    db.create_recipe(&sc, RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap();
    assert!(common::hits(&h, 1).await.is_empty());
}

// ---- the command ----

#[tokio::test(flavor = "multi_thread")]
async fn the_command_sends_what_is_due_exits_zero_and_does_not_repeat() {
    let (url, h) = common::receiver(200).await;
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open(&dir.path().join("mealier.db")).await.unwrap();
        let sc = common::scope(&db).await;
        let today = Clock::from_unix(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64, false).date;
        plan(&db, &sc, &today, "dinner", None, "Soup").await;
        db.create_webhook(scheduled(&format!("{url}/cmd"), "00:00", sc.household_id, true)).await.unwrap();
    }
    let run = || {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_mealier")).arg("run-scheduled").env("MEALIER_DATA_DIR", dir.path()).env_remove("TZ").output().unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr))
    };
    let (ok, text) = run();
    assert!(ok, "{text}");
    assert!(text.contains("sent 1"), "{text}");
    assert_eq!(common::hits(&h, 1).await.len(), 1);
    let (ok, text) = run();
    assert!(ok && text.contains("sent 0"), "{text}");
    assert_eq!(common::hits(&h, 2).await.len(), 1, "not twice");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_command_exits_non_zero_when_a_send_failed() {
    let (url, _h) = common::receiver(503).await;
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open(&dir.path().join("mealier.db")).await.unwrap();
        let sc = common::scope(&db).await;
        let today = Clock::from_unix(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64, false).date;
        plan(&db, &sc, &today, "dinner", None, "Soup").await;
        db.create_webhook(scheduled(&format!("{url}/cmd"), "00:00", sc.household_id, true)).await.unwrap();
    }
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_mealier")).arg("run-scheduled").env("MEALIER_DATA_DIR", dir.path()).env_remove("TZ").output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("failed 1"), "{}", String::from_utf8_lossy(&out.stdout));
}
