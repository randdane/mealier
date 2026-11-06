//! The one LLM client (spec, "LLM"): config from the environment, one `complete_json` call.
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use utoipa::ToSchema;

use crate::{Error, Result};

/// An API key. Debug prints a placeholder so it cannot reach a log by accident.
#[derive(Clone)]
pub struct Secret(String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Anthropic,
    OpenAi,
    OpenRouter,
}

/// What the model guessed; fills the edit form, never the recipe.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct NutritionEstimate {
    /// kcal per serving
    pub calories: Option<f64>,
    /// grams
    pub protein: Option<f64>,
    /// grams
    pub fat: Option<f64>,
    /// grams
    pub carbs: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub provider: Provider,
    pub model: String,
    api_key: Secret,
    /// `LLM_BASE_URL`: tests and proxies only.
    pub base_url: Option<String>,
}

impl LlmConfig {
    /// `None` when `LLM_PROVIDER` is unset. Fails at startup, never at first use.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Option<LlmConfig>> {
        use std::os::unix::fs::PermissionsExt;
        let var = |k: &str| get(k).filter(|v| !v.is_empty());
        let Some(p) = var("LLM_PROVIDER") else { return Ok(None) };
        let provider = match p.as_str() {
            "anthropic" => Provider::Anthropic,
            "openai" => Provider::OpenAi,
            "openrouter" => Provider::OpenRouter,
            _ => return Err(Error::Validation("LLM_PROVIDER must be anthropic, openai or openrouter".into())),
        };
        let model = var("LLM_MODEL").ok_or_else(|| Error::Validation("LLM_MODEL is required when LLM_PROVIDER is set".into()))?;
        let path = var("LLM_API_KEY_FILE").ok_or_else(|| Error::Validation("LLM_API_KEY_FILE is required when LLM_PROVIDER is set".into()))?;
        // Errors name the file, never its content.
        let mode = std::fs::metadata(&path).map_err(|e| Error::Validation(format!("cannot read LLM_API_KEY_FILE ({path}): {}", e.kind())))?.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(Error::Validation(format!("LLM_API_KEY_FILE ({path}) is readable by group or others (mode {:o}); chmod 600 it", mode & 0o777)));
        }
        let key = std::fs::read_to_string(&path).map_err(|e| Error::Validation(format!("cannot read LLM_API_KEY_FILE ({path}): {}", e.kind())))?;
        let key = key.trim_end_matches(['\n', '\r']);
        if key.is_empty() {
            return Err(Error::Validation(format!("LLM_API_KEY_FILE ({path}) is empty")));
        }
        Ok(Some(LlmConfig { provider, model, api_key: Secret(key.to_string()), base_url: var("LLM_BASE_URL") }))
    }
}

pub struct Llm {
    cfg: LlmConfig,
    http: reqwest::Client,
}

const TIMEOUT: Duration = Duration::from_secs(60);

impl Llm {
    pub fn new(cfg: LlmConfig) -> Llm {
        // No redirects: a hop must never carry the key to another host.
        let http = reqwest::Client::builder().timeout(TIMEOUT).redirect(reqwest::redirect::Policy::none()).build().expect("reqwest client");
        Llm { cfg, http }
    }

    pub fn from_env() -> Result<Option<Llm>> {
        Ok(LlmConfig::from_vars(|k| std::env::var(k).ok())?.map(Llm::new))
    }

    pub fn provider(&self) -> Provider {
        self.cfg.provider
    }

    pub fn model(&self) -> &str {
        &self.cfg.model
    }

    /// The settings page's "Test": a tiny prompt, so a wrong key or model shows up without a recipe.
    pub async fn test(&self) -> Result<()> {
        self.complete_json("You answer in JSON.", "Reply with ok set to true.", r#"{"ok":true}"#).await.map(|_| ())
    }

    /// Per-serving nutrition for a recipe, as the model guesses it. Not saved anywhere.
    pub async fn estimate_nutrition(&self, name: &str, ingredients: &[String]) -> Result<NutritionEstimate> {
        let system = "You estimate the nutrition of one serving of a recipe: calories in kcal; protein, fat and carbs in grams. Use null for a value you cannot estimate.";
        let user = format!("{name}\n\nIngredients:\n{}", ingredients.join("\n"));
        let v = self.complete_json(system, &user, r#"{"calories":0,"protein":0,"fat":0,"carbs":0}"#).await?;
        let n = |k: &str| crate::urlimport::first_number(&v[k]).filter(|n| n.is_finite() && *n >= 0.0);
        Ok(NutritionEstimate { calories: n("calories"), protein: n("protein"), fat: n("fat"), carbs: n("carbs") })
    }

    /// Asks for one JSON object. `schema_hint` is appended to the system prompt.
    pub async fn complete_json(&self, system: &str, user: &str, schema_hint: &str) -> Result<Value> {
        let system = format!("{system}\n\nReply with only a JSON object, no prose or code fences, shaped like: {schema_hint}");
        let (base, key) = (self.cfg.base_url.as_deref().map(|b| b.trim_end_matches('/')), &self.cfg.api_key.0);
        let req = match self.cfg.provider {
            Provider::Anthropic => self
                .http
                .post(format!("{}/v1/messages", base.unwrap_or("https://api.anthropic.com")))
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01")
                .body(json!({"model": self.cfg.model, "max_tokens": 4096, "system": system, "messages": [{"role": "user", "content": user}]}).to_string()),
            Provider::OpenAi | Provider::OpenRouter => {
                let default = if self.cfg.provider == Provider::OpenAi { "https://api.openai.com" } else { "https://openrouter.ai/api" };
                self.http
                    .post(format!("{}/v1/chat/completions", base.unwrap_or(default)))
                    .bearer_auth(key)
                    .body(json!({"model": self.cfg.model, "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}], "response_format": {"type": "json_object"}}).to_string())
            }
        };
        let fail = |what: &str| Error::Validation(format!("LLM request failed: {what}"));
        // reqwest's own error text carries the URL, never a header; the key is only ever in headers.
        let resp = req.header("content-type", "application/json").send().await.map_err(|e| fail(&e.without_url().to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(fail(&format!("the provider answered {status}")));
        }
        let body: Value = serde_json::from_slice(&resp.bytes().await.map_err(|_| fail("the reply was cut off"))?).map_err(|_| fail("the reply was not JSON"))?;
        let text = match self.cfg.provider {
            Provider::Anthropic => body["content"][0]["text"].as_str(),
            _ => body["choices"][0]["message"]["content"].as_str(),
        };
        extract_json(text.ok_or_else(|| fail("the reply had no text"))?).ok_or_else(|| fail("the reply held no JSON object"))
    }
}

/// The JSON object in a model's text: as is, or the span from the first `{` to the last `}` (fences, chatter).
fn extract_json(text: &str) -> Option<Value> {
    let t = text.trim();
    if let Ok(v @ Value::Object(_)) = serde_json::from_str(t) {
        return Some(v);
    }
    serde_json::from_str(&t[t.find('{')?..=t.rfind('}')?]).ok().filter(Value::is_object)
}
