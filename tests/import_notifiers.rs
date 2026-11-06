//! The Mealie importer's notifiers and scheduled webhooks (step 15). The fixture has none (it has neither
//! table), so synthetic rows are injected into a copy of the zip.
//!
//! Column names are from Mealie v3.28.0's models (`db/models/household/events.py` and `webhooks.py`) and
//! its live `/openapi.json`: `group_events_notifiers` (id, name, enabled, apprise_url, group_id,
//! household_id), `group_events_notifier_options` (id, event_notifier_id and one boolean per event) and
//! `webhook_urls` (id, group_id, household_id, enabled, name, url, webhook_type, scheduled_time, and the
//! unused legacy `time`). Not confirmed against a real backup row: how `scheduled_time` is written in
//! `database.json` (read leniently as `HH:MM[:SS[.f]]`).
mod common;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use mealier::{Db, ImportReport, apprise_to_webhook, import_mealie_backup};
use serde_json::{Value, json};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mealie_v3.28.0.zip")
}

/// The fixture with database.json edited by `edit`, and `files` added to the zip.
fn mutated(edit: impl Fn(&mut Value), files: &[(&str, &[u8])]) -> (tempfile::TempDir, PathBuf) {
    let mut src = zip::ZipArchive::new(std::fs::File::open(fixture()).unwrap()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("edited.zip");
    let mut out = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    for i in 0..src.len() {
        let mut f = src.by_index(i).unwrap();
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes).unwrap();
        let name = f.name().unwrap().to_string();
        if name == "database.json" {
            let mut v: Value = serde_json::from_slice(&bytes).unwrap();
            edit(&mut v);
            bytes = serde_json::to_vec(&v).unwrap();
        }
        out.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
        out.write_all(&bytes).unwrap();
    }
    for (name, bytes) in files {
        out.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
        out.write_all(bytes).unwrap();
    }
    out.finish().unwrap();
    (dir, path)
}

/// Appends a row; the fixture has neither table, so the table is made.
fn push(v: &mut Value, table: &str, row: Value) {
    v.as_object_mut().unwrap().entry(table).or_insert_with(|| json!([])).as_array_mut().unwrap().push(row);
}

async fn import(path: &Path) -> (Db, ImportReport, tempfile::TempDir) {
    let db = Db::open_memory().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let report = import_mealie_backup(&db, path, dir.path()).await.unwrap();
    (db, report, dir)
}

const HOUSEHOLD: &str = "bdd39ab8b477457a89bcdf4d68e12d0d";
const GROUP: &str = "6a1a76f2eef844d7b079f8421d1d7b6d";


const SECRET: &str = "tk_s3cr3t_value";

fn notifier(id: &str, name: &str, apprise: &str, enabled: bool) -> Value {
    json!({"id": id, "name": name, "enabled": enabled, "apprise_url": apprise, "group_id": GROUP, "household_id": HOUSEHOLD})
}

/// An options row with these (and only these) booleans on.
fn options(notifier_id: &str, on: &[&str]) -> Value {
    let mut row = json!({"id": format!("opt-{notifier_id}"), "event_notifier_id": notifier_id});
    for e in on {
        row[*e] = json!(true);
    }
    row
}

/// The whole report as text: nothing secret may be in it.
fn text(r: &ImportReport) -> String {
    format!("{r:?}")
}

/// The report lines about notifiers and webhooks (the fixture adds lines of its own).
fn lines(r: &ImportReport) -> Vec<&String> {
    r.skipped.iter().filter(|s| s.starts_with("notifier ") || s.starts_with("scheduled webhook") || s.starts_with("scheduled webhooks")).collect()
}

async fn hooks(db: &Db) -> Vec<mealier::Webhook> {
    db.list_webhooks().await.unwrap()
}

// ---------- the Apprise URL conversion, literally ----------

fn ok(apprise: &str) -> (String, String, bool) {
    let c = apprise_to_webhook(apprise).unwrap_or_else(|e| panic!("{apprise}: {e}"));
    (c.kind.to_string(), c.url, c.dropped_query)
}

#[test]
fn ntfy_urls_become_publish_urls() {
    let n = |u: &str| ("ntfy".to_string(), u.to_string(), false);
    assert_eq!(ok("ntfys://ntfy.example.com/mealier"), n("https://ntfy.example.com/mealier"));
    assert_eq!(ok("ntfy://ntfy.lan:8080/mealier"), n("http://ntfy.lan:8080/mealier"));
    assert_eq!(ok("ntfys://ntfy.example.com:8443/mealier"), n("https://ntfy.example.com:8443/mealier"));
    assert_eq!(ok("NTFYS://Ntfy.Example.com/Mealier"), n("https://ntfy.example.com/Mealier"), "scheme and host are case-insensitive, the topic is not");
    assert_eq!(ok("ntfy://ntfy.lan/mealier/"), n("http://ntfy.lan/mealier"), "a trailing slash is not a second topic");
}

#[test]
fn ntfy_credentials_stay_in_the_url_as_basic_auth() {
    let n = |u: &str| ("ntfy".to_string(), u.to_string(), false);
    assert_eq!(ok("ntfys://alice:hunter2@ntfy.example.com/mealier"), n("https://alice:hunter2@ntfy.example.com/mealier"));
    assert_eq!(ok("ntfy://alice:hunter2@ntfy.lan:8080/mealier"), n("http://alice:hunter2@ntfy.lan:8080/mealier"));
    assert_eq!(ok("ntfys://al%40ice:p%3Ass@ntfy.example.com/mealier"), n("https://al%40ice:p%3Ass@ntfy.example.com/mealier"), "percent-encoding is kept as it was");
}

#[test]
fn a_bare_ntfy_topic_means_the_hosted_service() {
    let n = |u: &str| ("ntfy".to_string(), u.to_string(), false);
    assert_eq!(ok("ntfy://mealier"), n("https://ntfy.sh/mealier"));
    assert_eq!(ok("ntfys://mealier"), n("https://ntfy.sh/mealier"));
}

#[test]
fn a_query_is_dropped_and_said_so() {
    assert_eq!(ok("ntfys://ntfy.example.com/mealier?priority=high&tags=a"), ("ntfy".into(), "https://ntfy.example.com/mealier".into(), true));
    assert_eq!(ok("jsons://hooks.example.com/in?-X-Token=abc"), ("json".into(), "https://hooks.example.com/in".into(), true));
}

#[test]
fn json_urls_become_json_webhooks() {
    let j = |u: &str| ("json".to_string(), u.to_string(), false);
    assert_eq!(ok("json://hooks.lan:8080/path/x"), j("http://hooks.lan:8080/path/x"));
    assert_eq!(ok("jsons://hooks.example.com/a/b"), j("https://hooks.example.com/a/b"));
    assert_eq!(ok("jsons://bob:pw@hooks.example.com/a"), j("https://bob:pw@hooks.example.com/a"));
    assert_eq!(ok("json://hooks.lan"), j("http://hooks.lan/"));
}

#[test]
fn what_cannot_be_converted_says_why_without_quoting_the_url() {
    for (url, must_name) in [
        ("gotify://gotify.example.com/tk_s3cr3t_value", Some("gotify")),
        ("mailto://user:tk_s3cr3t_value@gmail.com", Some("mailto")),
        ("discord://123/tk_s3cr3t_value", Some("discord")),
        ("ntfys://ntfy.example.com/one/two?x=tk_s3cr3t_value", Some("several")),
        ("ntfys://tk_s3cr3t_value@ntfy.example.com/mealier", Some("password")),
        ("not a url tk_s3cr3t_value", None),
        ("ntfys://", None),
        ("json://", None),
        ("", None),
    ] {
        let e = apprise_to_webhook(url).expect_err(url);
        assert!(!e.contains(SECRET) && !e.contains("example.com") && !e.contains("//"), "{url}: {e}");
        if let Some(w) = must_name {
            assert!(e.contains(w), "{url}: {e}");
        }
    }
}

// ---------- notifiers ----------

#[tokio::test]
async fn an_ntfy_notifier_becomes_an_ntfy_webhook_with_its_events() {
    let url = format!("ntfys://bob:{SECRET}@ntfy.example.com/mealier");
    let (_dir, zip) = mutated(
        |v| {
            push(v, "group_events_notifiers", notifier("n1", "Phone", &url, true));
            push(v, "group_events_notifier_options", options("n1", &["recipe_created", "cookbook_deleted", "mealplan_entry_created", "user_signup", "tag_created"]));
        },
        &[],
    );
    let (db, report, _d) = import(&zip).await;
    assert_eq!((report.webhooks, report.scheduled_webhooks), (1, 0));
    let w = &hooks(&db).await[0];
    assert_eq!((w.name.as_str(), w.kind.as_str(), w.url.as_str(), w.enabled), ("Phone", "ntfy", format!("https://bob:{SECRET}@ntfy.example.com/mealier").as_str(), true));
    assert_eq!(w.events, ["recipe_created", "mealplan_entry_created", "cookbook_deleted"], "in mealier's order");
    let group = db.groups().await.unwrap().into_iter().find(|g| g.id == w.group_id.unwrap()).unwrap();
    assert!(!group.name.is_empty(), "the notifier's group became the webhook's group filter");

    let unmapped: Vec<&String> = lines(&report);
    assert!(unmapped[0].contains("user_signup"));
    assert_eq!(unmapped.len(), 1, "one line for the options mealier has no event for: {:?}", report.skipped);
    assert!(unmapped[0].contains("tag_created") && unmapped[0].contains("Phone"), "{unmapped:?}");
    assert!(!text(&report).contains(SECRET), "the report has a secret in it");
}

#[tokio::test]
async fn a_disabled_json_notifier_stays_disabled() {
    let (_dir, zip) = mutated(
        |v| {
            push(v, "group_events_notifiers", notifier("n1", "Hook", &format!("jsons://hooks.example.com/{SECRET}"), false));
            push(v, "group_events_notifier_options", options("n1", &["shopping_list_updated"]));
            push(v, "group_events_notifiers", notifier("n2", "Plain", "json://hooks.lan:8080/in", true));
            push(v, "group_events_notifier_options", options("n2", &["recipe_deleted"]));
        },
        &[],
    );
    let (db, report, _d) = import(&zip).await;
    assert_eq!(report.webhooks, 2);
    let hs = hooks(&db).await;
    let by = |n: &str| hs.iter().find(|h| h.name == n).unwrap();
    assert_eq!((by("Hook").kind.as_str(), by("Hook").url.as_str(), by("Hook").enabled), ("json", format!("https://hooks.example.com/{SECRET}").as_str(), false));
    assert_eq!((by("Plain").url.as_str(), by("Plain").events.as_slice()), ("http://hooks.lan:8080/in", &["recipe_deleted".to_string()][..]));
    assert!(lines(&report).is_empty(), "{:?}", report.skipped);
}

/// Mealie's option field -> mealier event. Every other option of Mealie v3.28 has no equivalent.
const MAPPED: [(&str, &str); 10] = [
    ("recipe_created", "recipe_created"),
    ("recipe_updated", "recipe_updated"),
    ("recipe_deleted", "recipe_deleted"),
    ("mealplan_entry_created", "mealplan_entry_created"),
    ("shopping_list_created", "shopping_list_created"),
    ("shopping_list_updated", "shopping_list_updated"),
    ("shopping_list_deleted", "shopping_list_deleted"),
    ("cookbook_created", "cookbook_created"),
    ("cookbook_updated", "cookbook_updated"),
    ("cookbook_deleted", "cookbook_deleted"),
];

const UNMAPPED: [&str; 17] = [
    "test_message", "webhook_task", "user_signup", "data_migrations", "data_export", "data_import", "mealplan_entry_updated", "mealplan_entry_deleted", "tag_created", "tag_updated", "tag_deleted",
    "category_created", "category_updated", "category_deleted", "label_created", "label_updated", "label_deleted",
];

#[tokio::test]
async fn each_mealie_option_maps_to_its_own_event() {
    let (_dir, zip) = mutated(
        |v| {
            for (i, (opt, _)) in MAPPED.iter().enumerate() {
                let id = format!("n{i}");
                push(v, "group_events_notifiers", notifier(&id, opt, "json://hooks.lan/in", true));
                push(v, "group_events_notifier_options", options(&id, &[opt]));
            }
        },
        &[],
    );
    let (db, report, _d) = import(&zip).await;
    assert_eq!(report.webhooks, 10);
    for (opt, event) in MAPPED {
        let h = hooks(&db).await.into_iter().find(|h| h.name == opt).unwrap();
        assert_eq!(h.events, [event], "{opt}");
    }
    assert!(lines(&report).is_empty(), "{:?}", report.skipped);
}

#[tokio::test]
async fn options_with_no_event_are_reported_and_a_notifier_with_nothing_left_is_skipped() {
    let (_dir, zip) = mutated(
        |v| {
            push(v, "group_events_notifiers", notifier("n1", "Everything", "json://hooks.lan/in", true));
            push(v, "group_events_notifier_options", options("n1", &[&MAPPED.map(|m| m.0)[..], &UNMAPPED].concat()));
            push(v, "group_events_notifiers", notifier("n2", "Only tags", "json://hooks.lan/two", true));
            push(v, "group_events_notifier_options", options("n2", &["tag_created", "label_deleted"]));
            push(v, "group_events_notifiers", notifier("n3", "No options row", "json://hooks.lan/three", true));
        },
        &[],
    );
    let (db, report, _d) = import(&zip).await;
    assert_eq!(report.webhooks, 1);
    assert_eq!(hooks(&db).await.len(), 1);
    assert_eq!(hooks(&db).await[0].events.len(), 10);
    let mine: Vec<&String> = report.skipped.iter().filter(|s| s.starts_with("notifier \"Everything\"")).collect();
    assert_eq!(mine.len(), 1, "{:?}", report.skipped);
    for o in UNMAPPED {
        assert!(mine[0].contains(o), "{o} missing from {mine:?}");
    }
    assert!(report.skipped.iter().any(|s| s.contains("Only tags") && s.contains("no event")), "{:?}", report.skipped);
    assert!(report.skipped.iter().any(|s| s.contains("No options row") && s.contains("no event")), "{:?}", report.skipped);
}

#[tokio::test]
async fn other_schemes_are_skipped_naming_only_the_scheme() {
    let (_dir, zip) = mutated(
        |v| {
            push(v, "group_events_notifiers", notifier("n1", "Chat", &format!("discord://12345/{SECRET}"), true));
            push(v, "group_events_notifier_options", options("n1", &["recipe_created"]));
            push(v, "group_events_notifiers", notifier("n2", "Mail", &format!("mailto://u:{SECRET}@gmail.com"), true));
            push(v, "group_events_notifier_options", options("n2", &["recipe_created"]));
            push(v, "group_events_notifiers", notifier("n3", "Odd", &format!("ntfys://{SECRET}@ntfy.example.com/t"), true));
            push(v, "group_events_notifier_options", options("n3", &["recipe_created"]));
        },
        &[],
    );
    let (db, report, _d) = import(&zip).await;
    assert_eq!(report.webhooks, 0);
    assert!(hooks(&db).await.is_empty());
    assert_eq!(report.skipped.iter().filter(|s| s.contains("discord")).count(), 1, "{:?}", report.skipped);
    assert_eq!(report.skipped.iter().filter(|s| s.contains("mailto")).count(), 1);
    assert_eq!(lines(&report).len(), 3, "{:?}", report.skipped);
    let t = text(&report);
    assert!(!t.contains(SECRET) && !t.contains("12345") && !t.contains("gmail.com") && !t.contains("ntfy.example.com"), "{t}");
}

#[tokio::test]
async fn a_dropped_query_is_reported_without_the_url() {
    let (_dir, zip) = mutated(
        |v| {
            push(v, "group_events_notifiers", notifier("n1", "Phone", &format!("ntfys://ntfy.example.com/mealier?priority=high&auth={SECRET}"), true));
            push(v, "group_events_notifier_options", options("n1", &["recipe_created"]));
        },
        &[],
    );
    let (db, report, _d) = import(&zip).await;
    assert_eq!(hooks(&db).await[0].url, "https://ntfy.example.com/mealier");
    assert_eq!(report.skipped.iter().filter(|s| s.contains("Phone") && s.contains("query")).count(), 1, "{:?}", report.skipped);
    assert!(!text(&report).contains(SECRET));
}

// ---------- scheduled webhooks ----------

fn scheduled(name: &str, url: &str, time: Value) -> Value {
    json!({"id": format!("s-{name}"), "group_id": GROUP, "household_id": HOUSEHOLD, "enabled": true, "name": name, "url": url, "webhook_type": "mealplan", "scheduled_time": time, "time": "00:00"})
}

#[tokio::test]
async fn a_scheduled_webhook_comes_in_disabled_with_its_time_and_household() {
    let (_dir, zip) = mutated(
        |v| {
            push(v, "webhook_urls", scheduled("Morning", &format!("https://hooks.example.com/{SECRET}"), json!("07:30:00")));
            push(v, "webhook_urls", scheduled("Precise", "http://hooks.lan/x", json!("18:05:09.123456")));
            push(v, "webhook_urls", scheduled("Zoned", "http://hooks.lan/y", json!("06:45:00+00:00")));
        },
        &[],
    );
    let (db, report, _d) = import(&zip).await;
    assert_eq!((report.scheduled_webhooks, report.webhooks), (3, 0));
    let hs = hooks(&db).await;
    let by = |n: &str| hs.iter().find(|h| h.name == n).unwrap();
    let w = by("Morning");
    assert_eq!((w.kind.as_str(), w.time_of_day.as_deref(), w.enabled, w.url.as_str()), ("scheduled", Some("07:30"), false, format!("https://hooks.example.com/{SECRET}").as_str()));
    assert_eq!(by("Precise").time_of_day.as_deref(), Some("18:05"));
    assert_eq!(by("Zoned").time_of_day.as_deref(), Some("06:45"));
    let household = db.households().await.unwrap().into_iter().find(|h| Some(h.id) == w.household_id).expect("household");
    assert_eq!(Some(household.group_id), w.group_id);
    assert_eq!(report.skipped.iter().filter(|s| s.contains("UTC")).count(), 1, "one line about the timezone, not one per webhook: {:?}", report.skipped);
    assert!(!text(&report).contains(SECRET));
}

#[tokio::test]
async fn a_scheduled_webhook_without_a_household_goes_to_the_groups_first() {
    let (_dir, zip) = mutated(
        |v| {
            let mut row = scheduled("NoHousehold", "http://hooks.lan/x", json!("07:30:00"));
            row.as_object_mut().unwrap().remove("household_id");
            push(v, "webhook_urls", row);
        },
        &[],
    );
    let (db, _r, _d) = import(&zip).await;
    assert!(hooks(&db).await[0].household_id.is_some());
}

#[tokio::test]
async fn unusable_scheduled_webhooks_are_skipped_and_the_legacy_time_is_the_fallback() {
    let (_dir, zip) = mutated(
        |v| {
            push(v, "webhook_urls", scheduled("BadUrl", &format!("ftp://hooks.example.com/{SECRET}"), json!("07:30:00")));
            push(v, "webhook_urls", scheduled("NoUrl", "", json!("07:30:00")));
            let mut no_time = scheduled("NoTime", "http://hooks.lan/a", Value::Null);
            no_time["time"] = Value::Null;
            push(v, "webhook_urls", no_time);
            let mut legacy = scheduled("Legacy", "http://hooks.lan/b", Value::Null);
            legacy["time"] = json!("21:10");
            push(v, "webhook_urls", legacy);
            let mut garbage = scheduled("Garbage", "http://hooks.lan/c", json!("soon"));
            garbage["time"] = json!("later");
            push(v, "webhook_urls", garbage);
        },
        &[],
    );
    let (db, report, _d) = import(&zip).await;
    let names: Vec<String> = hooks(&db).await.into_iter().map(|h| h.name).collect();
    assert_eq!(names, ["Legacy"]);
    assert_eq!(report.scheduled_webhooks, 1);
    for n in ["BadUrl", "NoUrl", "NoTime", "Garbage"] {
        assert_eq!(report.skipped.iter().filter(|s| s.contains(n)).count(), 1, "{n}: {:?}", report.skipped);
    }
    assert!(report.skipped.iter().any(|s| s.contains("BadUrl") && s.contains("ftp")));
    assert!(!text(&report).contains(SECRET));
    assert_eq!(hooks(&db).await[0].time_of_day.as_deref(), Some("21:10"));
}

#[tokio::test]
async fn the_fixture_alone_has_neither() {
    let (db, report, _d) = import(&fixture()).await;
    assert_eq!((report.webhooks, report.scheduled_webhooks), (0, 0));
    assert!(hooks(&db).await.is_empty());
}

#[tokio::test]
async fn an_imported_scheduled_webhook_is_not_sent_until_enabled() {
    let (url, h) = common::receiver(200).await;
    let (_dir, zip) = mutated(|v| push(v, "webhook_urls", scheduled("Morning", &format!("{url}/plan"), json!("00:00:00"))), &[]);
    let (db, _r, _d) = import(&zip).await;
    let clock = mealier::Clock { date: "2026-11-06".into(), minutes: 23 * 60 };
    assert_eq!(db.run_scheduled(&clock).await.unwrap().sent, 0);
    assert!(common::hits(&h, 1).await.is_empty());
}
