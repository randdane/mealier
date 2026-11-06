//! URL import: fetch a page, run the importer chain, map the JSON-LD to a recipe.
use std::future::Future;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use scraper::{ElementRef, Html, Selector};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::fetch::Fetcher;
use crate::{Db, Error, Food, IngredientInput, Nutrition, Recipe, RecipeInput, Result, Unit};

pub struct Page {
    pub url: String,
    pub final_url: String,
    pub html: String,
}

/// Turns a page into schema.org Recipe JSON-LD, or `None` when it has nothing to offer.
pub trait Importer {
    fn name(&self) -> &str;
    fn import(&self, page: &Page) -> impl Future<Output = Result<Option<Value>>> + Send;
}

pub struct JsonLd;
pub struct Microdata;

impl Importer for JsonLd {
    fn name(&self) -> &str {
        "jsonld"
    }
    async fn import(&self, page: &Page) -> Result<Option<Value>> {
        let doc = Html::parse_document(&page.html);
        let sel = Selector::parse(r#"script[type="application/ld+json"]"#).unwrap();
        for s in doc.select(&sel) {
            let Ok(v) = serde_json::from_str::<Value>(s.text().collect::<String>().trim()) else { continue };
            if find_recipe(&v).is_some() {
                return Ok(Some(v));
            }
        }
        Ok(None)
    }
}

impl Importer for Microdata {
    fn name(&self) -> &str {
        "microdata"
    }
    async fn import(&self, page: &Page) -> Result<Option<Value>> {
        let doc = Html::parse_document(&page.html);
        let sel = Selector::parse("[itemscope][itemtype]").unwrap();
        Ok(doc.select(&sel).filter(|e| e.value().attr("itemtype").is_some_and(|t| t.contains("schema.org/Recipe"))).map(item).next())
    }
}

/// An itemscope element as a JSON object. Properties of nested items stay in their own item.
fn item(el: ElementRef) -> Value {
    let mut obj = serde_json::Map::new();
    if let Some(t) = el.value().attr("itemtype") {
        obj.insert("@type".into(), json!(t.rsplit('/').next().unwrap_or(t)));
    }
    fn walk(el: ElementRef, obj: &mut serde_json::Map<String, Value>) {
        for c in el.children().filter_map(ElementRef::wrap) {
            let v = c.value();
            if let Some(prop) = v.attr("itemprop") {
                let val = if v.attr("itemscope").is_some() { item(c) } else { json!(literal(c)) };
                for p in prop.split_whitespace() {
                    match obj.entry(p).or_insert(Value::Null) {
                        Value::Null => {
                            obj.insert(p.into(), val.clone());
                        }
                        Value::Array(a) => a.push(val.clone()),
                        old => *old = json!([old.take(), val.clone()]),
                    }
                }
            } else if v.attr("itemscope").is_none() {
                walk(c, obj);
            }
        }
    }
    walk(el, &mut obj);
    Value::Object(obj)
}

fn literal(e: ElementRef) -> String {
    let v = e.value();
    let attr = match v.name() {
        "meta" => "content",
        "img" | "source" | "audio" | "video" | "embed" | "iframe" | "track" => "src",
        "a" | "area" | "link" => "href",
        "time" => "datetime",
        "data" | "meter" => "value",
        _ => "",
    };
    match v.attr(attr).filter(|_| !attr.is_empty()) {
        Some(a) => a.trim().to_string(),
        None => e.text().collect::<Vec<_>>().join(" ").split_whitespace().collect::<Vec<_>>().join(" "),
    }
}

/// Runs an external command: `{url, html}` JSON on stdin, JSON-LD on stdout.
pub struct Exec {
    argv: Vec<String>,
    timeout: Duration,
}

const EXEC_STDOUT_CAP: usize = 2 * 1024 * 1024;
const EXEC_STDERR_CAP: usize = 64 * 1024;

impl Exec {
    pub fn new(argv: Vec<String>) -> Exec {
        Exec { argv, timeout: Duration::from_secs(20) }
    }
    pub fn with_timeout(mut self, t: Duration) -> Exec {
        self.timeout = t;
        self
    }
}

/// Up to `cap` bytes, then the rest is drained and dropped so the child never blocks on a full pipe.
async fn read_capped(mut r: impl tokio::io::AsyncRead + Unpin, cap: usize) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let _ = (&mut r).take(cap as u64).read_to_end(&mut buf).await;
    let more = tokio::io::copy(&mut r, &mut tokio::io::sink()).await.unwrap_or(0) > 0;
    (buf, more)
}

impl Importer for Exec {
    fn name(&self) -> &str {
        &self.argv[0]
    }
    async fn import(&self, page: &Page) -> Result<Option<Value>> {
        let mut child = tokio::process::Command::new(&self.argv[0])
            .args(&self.argv[1..])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()?;
        let pid = child.id().unwrap_or(0) as i32;
        let input = json!({ "url": page.final_url, "html": page.html }).to_string();
        let mut stdin = child.stdin.take().unwrap();
        let (out, err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
        let run = async {
            // A plugin that ignores stdin closes it early; that is not our error.
            let feed = async move {
                let _ = stdin.write_all(input.as_bytes()).await;
                drop(stdin);
            };
            let ((_, (out, out_more), (err, _)), status) =
                tokio::join!(async { tokio::join!(feed, read_capped(out, EXEC_STDOUT_CAP), read_capped(err, EXEC_STDERR_CAP)) }, child.wait());
            (out, out_more, err, status)
        };
        let (out, out_more, err, status) = match tokio::time::timeout(self.timeout, run).await {
            Ok(r) => r,
            Err(_) => {
                // SAFETY: killpg on the group we created with process_group(0).
                if pid > 0 {
                    unsafe { libc::killpg(pid, libc::SIGKILL) };
                }
                let _ = child.wait().await;
                eprintln!("mealier: exec importer {} timed out after {:?}", self.name(), self.timeout);
                return Ok(None);
            }
        };
        let status = status?;
        if !status.success() {
            eprintln!("mealier: exec importer {} exited {status}: {}", self.name(), String::from_utf8_lossy(&err));
            return Ok(None);
        }
        if out_more {
            eprintln!("mealier: exec importer {} wrote more than 2 MB", self.name());
            return Ok(None);
        }
        Ok(serde_json::from_slice::<Value>(&out).ok().filter(|v| find_recipe(v).is_some()))
    }
}

/// Dispatch without `dyn`: the trait uses native async fn.
pub enum AnyImporter {
    JsonLd(JsonLd),
    Microdata(Microdata),
    Exec(Exec),
}

impl Importer for AnyImporter {
    fn name(&self) -> &str {
        match self {
            Self::JsonLd(i) => i.name(),
            Self::Microdata(i) => i.name(),
            Self::Exec(i) => i.name(),
        }
    }
    async fn import(&self, page: &Page) -> Result<Option<Value>> {
        match self {
            Self::JsonLd(i) => i.import(page).await,
            Self::Microdata(i) => i.import(page).await,
            Self::Exec(i) => i.import(page).await,
        }
    }
}

/// Importers in order; the first that returns a recipe wins. An importer that errors is logged and skipped.
pub struct Chain(pub Vec<AnyImporter>);

impl Chain {
    /// `importers` is the comma list from `MEALIER_IMPORTERS`; `exec` the JSON argv arrays
    /// from `MEALIER_EXEC_IMPORTERS`, placed where `exec` appears in the list.
    pub fn build(importers: &str, exec: Option<&str>) -> std::result::Result<Chain, String> {
        let execs: Vec<Vec<String>> = match exec.filter(|s| !s.trim().is_empty()) {
            Some(s) => serde_json::from_str(s).map_err(|e| format!("MEALIER_EXEC_IMPORTERS must be a JSON array of argv arrays: {e}"))?,
            None => Vec::new(),
        };
        if execs.iter().any(|a| a.is_empty()) {
            return Err("MEALIER_EXEC_IMPORTERS: an argv array is empty".into());
        }
        let mut v = Vec::new();
        for n in importers.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            match n {
                "jsonld" => v.push(AnyImporter::JsonLd(JsonLd)),
                "microdata" => v.push(AnyImporter::Microdata(Microdata)),
                "exec" => v.extend(execs.iter().cloned().map(|a| AnyImporter::Exec(Exec::new(a)))),
                // step 9: llm importer
                other => return Err(format!("MEALIER_IMPORTERS: unknown importer '{other}'")),
            }
        }
        Ok(Chain(v))
    }

    pub async fn run(&self, page: &Page) -> Option<Value> {
        for i in &self.0 {
            match i.import(page).await {
                Ok(Some(v)) => return Some(v),
                Ok(None) => {}
                Err(e) => eprintln!("mealier: importer {} failed: {e}", i.name()),
            }
        }
        None
    }
}

/// Fetcher plus chain: everything `POST /api/recipes/import` needs.
pub struct ImportService {
    fetcher: Fetcher,
    chain: Chain,
}

impl Default for ImportService {
    fn default() -> Self {
        Self::new(Fetcher::default(), Chain::build("jsonld,microdata", None).unwrap())
    }
}

impl ImportService {
    pub fn new(fetcher: Fetcher, chain: Chain) -> Self {
        ImportService { fetcher, chain }
    }

    pub fn from_env() -> std::result::Result<Self, String> {
        let list = std::env::var("MEALIER_IMPORTERS").unwrap_or_else(|_| "jsonld,microdata".into());
        let exec = std::env::var("MEALIER_EXEC_IMPORTERS").ok();
        Ok(Self::new(Fetcher::default(), Chain::build(&list, exec.as_deref())?))
    }

    pub async fn import(&self, db: &Db, images_dir: &Path, url: &str) -> Result<Recipe> {
        self.import_inner(db, images_dir, url).await.inspect_err(|e| {
            // step 7: raise import_failed
            eprintln!("mealier: import_failed url={url} reason={e}");
        })
    }

    async fn import_inner(&self, db: &Db, images_dir: &Path, url: &str) -> Result<Recipe> {
        let f = self.fetcher.get(url).await?;
        let page = Page { url: url.into(), final_url: f.final_url.to_string(), html: String::from_utf8_lossy(&f.body).into_owned() };
        let ld = self.chain.run(&page).await.ok_or_else(|| Error::Validation("no recipe found on that page".into()))?;
        let (units, foods) = (db.units().await?, db.foods().await?);
        let (input, image) = map_jsonld(&ld, &page.final_url, &units, &foods).ok_or_else(|| Error::Validation("no recipe found on that page".into()))?;
        let recipe = db.create_recipe(input).await?;
        if let Some(img) = image {
            // A missing or bad image never fails the import.
            if let Err(e) = self.attach_image(db, images_dir, recipe.id, &img).await {
                eprintln!("mealier: import image skipped url={img} reason={e}");
            }
        }
        Ok(recipe)
    }

    async fn attach_image(&self, db: &Db, dir: &Path, recipe_id: i64, url: &str) -> Result<()> {
        let bytes = self.fetcher.get(url).await?.body;
        let (mime, ext) = match bytes.as_slice() {
            [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, ..] => ("image/png", "png"),
            [0xff, 0xd8, 0xff, ..] => ("image/jpeg", "jpg"),
            [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => ("image/webp", "webp"),
            _ => return Err(Error::Validation("not a png, jpeg or webp image".into())),
        };
        let hash = crate::import::image_hash(&bytes);
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join(format!("{hash}.{ext}")), &bytes)?;
        let pool = db.pool();
        sqlx::query("INSERT OR IGNORE INTO images (hash, mime) VALUES (?, ?)").bind(&hash).bind(mime).execute(pool).await?;
        sqlx::query("INSERT INTO recipe_images (recipe_id, hash) VALUES (?, ?)").bind(recipe_id).bind(&hash).execute(pool).await?;
        Ok(())
    }
}

fn has_type(v: &Value, t: &str) -> bool {
    match &v["@type"] {
        Value::String(s) => s == t,
        Value::Array(a) => a.iter().any(|x| x == t),
        _ => false,
    }
}

/// The Recipe node in a JSON-LD document: top-level object, array, or inside `@graph`.
pub fn find_recipe(v: &Value) -> Option<&Value> {
    match v {
        Value::Array(a) => a.iter().find_map(find_recipe),
        Value::Object(_) if has_type(v, "Recipe") => Some(v),
        Value::Object(o) => o.get("@graph").and_then(find_recipe),
        _ => None,
    }
}

fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        Value::Array(a) => a.iter().flat_map(strings).collect(),
        _ => Vec::new(),
    }
}

fn steps(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.extend(s.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from)),
        Value::Array(a) => a.iter().for_each(|x| steps(x, out)),
        Value::Object(o) if has_type(v, "HowToSection") => {
            if let Some(l) = o.get("itemListElement") {
                steps(l, out);
            }
        }
        Value::Object(o) => {
            // HowToStep: prefer the full text over the short name.
            if let Some(t) = o.get("text").or_else(|| o.get("name")) {
                steps(t, out);
            }
        }
        _ => {}
    }
}

fn first_image(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => a.iter().find_map(first_image),
        Value::Object(o) => o.get("url").and_then(first_image),
        _ => None,
    }
}

/// "250 calories", "12.5 g", 12 -> the first number in it.
fn first_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let start = s.find(|c: char| c.is_ascii_digit())?;
            let t = &s[start..];
            let end = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
            t[..end].trim_end_matches('.').parse().ok()
        }
        _ => None,
    }
}

/// JSON-LD Recipe -> RecipeInput, plus the absolute URL of its first image.
pub fn map_jsonld(ld: &Value, final_url: &str, units: &[Unit], foods: &[Food]) -> Option<(RecipeInput, Option<String>)> {
    let r = find_recipe(ld)?;
    let text = |k: &str| r[k].as_str().map(|s| s.trim().to_string()).unwrap_or_default();
    let ingredients = strings_lines(&r["recipeIngredient"]).iter().map(|l| crate::web::parsed_input(l, units, foods)).collect::<Vec<IngredientInput>>();
    let mut st = Vec::new();
    steps(&r["recipeInstructions"], &mut st);
    let n = &r["nutrition"];
    let nutrition = n.is_object().then(|| Nutrition {
        calories: first_number(&n["calories"]),
        protein_g: first_number(&n["proteinContent"]),
        fat_g: first_number(&n["fatContent"]),
        carbs_g: first_number(&n["carbohydrateContent"]),
    });
    let image = first_image(&r["image"]).and_then(|i| url::Url::parse(final_url).ok()?.join(&i).ok()).map(String::from);
    let input = RecipeInput {
        name: text("name"),
        description: text("description"),
        source_url: Some(final_url.to_string()),
        nutrition,
        tags: strings(&r["keywords"]),
        categories: strings(&r["recipeCategory"]),
        ingredients,
        steps: st,
        ..Default::default()
    };
    Some((input, image))
}

/// Ingredient lines are whole strings; commas inside them must survive.
fn strings_lines(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.trim().to_string()].into_iter().filter(|s| !s.is_empty()).collect(),
        Value::Array(a) => a.iter().flat_map(strings_lines).collect(),
        _ => Vec::new(),
    }
}
