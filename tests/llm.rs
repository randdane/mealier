use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use mealier::{Llm, LlmConfig, Provider};
use serde_json::{Value, json};

const KEY: &str = "sk-test-KEY-123";

fn key_file(mode: u32) -> std::path::PathBuf {
    let p = tempfile::tempdir().unwrap().keep().join("key");
    std::fs::write(&p, format!("{KEY}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
    p
}

fn vars(pairs: &[(&str, String)]) -> impl Fn(&str) -> Option<String> {
    let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    move |k| m.get(k).cloned()
}

// S29
#[test]
fn unset_provider_disables_llm() {
    assert!(LlmConfig::from_vars(vars(&[])).unwrap().is_none());
}

#[test]
fn model_and_key_file_are_required() {
    let key = key_file(0o600).to_str().unwrap().to_string();
    let e = LlmConfig::from_vars(vars(&[("LLM_PROVIDER", "openai".into()), ("LLM_API_KEY_FILE", key)])).unwrap_err().to_string();
    assert!(e.contains("LLM_MODEL"), "{e}");
    let e = LlmConfig::from_vars(vars(&[("LLM_PROVIDER", "openai".into()), ("LLM_MODEL", "m".into())])).unwrap_err().to_string();
    assert!(e.contains("LLM_API_KEY_FILE"), "{e}");
    let e = LlmConfig::from_vars(vars(&[("LLM_PROVIDER", "openai".into()), ("LLM_MODEL", "m".into()), ("LLM_API_KEY_FILE", "/nonexistent/key".into())])).unwrap_err().to_string();
    assert!(e.contains("LLM_API_KEY_FILE"), "{e}");
    let e = LlmConfig::from_vars(vars(&[("LLM_PROVIDER", "gemini".into()), ("LLM_MODEL", "m".into())])).unwrap_err().to_string();
    assert!(e.contains("LLM_PROVIDER"), "{e}");
}

#[test]
fn a_group_or_world_readable_key_file_is_refused() {
    for mode in [0o644, 0o640, 0o604] {
        let key = key_file(mode).to_str().unwrap().to_string();
        let e = LlmConfig::from_vars(vars(&[("LLM_PROVIDER", "anthropic".into()), ("LLM_MODEL", "m".into()), ("LLM_API_KEY_FILE", key)])).unwrap_err().to_string();
        assert!(e.contains("LLM_API_KEY_FILE") && !e.contains(KEY), "{mode:o}: {e}");
    }
}

#[test]
fn debug_output_never_contains_the_key() {
    let key = key_file(0o600).to_str().unwrap().to_string();
    let cfg = LlmConfig::from_vars(vars(&[("LLM_PROVIDER", "anthropic".into()), ("LLM_MODEL", "claude-x".into()), ("LLM_API_KEY_FILE", key)])).unwrap().unwrap();
    assert_eq!((cfg.provider, cfg.model.as_str()), (Provider::Anthropic, "claude-x"));
    let dbg = format!("{cfg:?}");
    assert!(!dbg.contains("sk-test") && dbg.contains("claude-x"), "{dbg}");
}

// S30
type Seen = Arc<Mutex<Vec<(String, HeaderMap, Value)>>>;

async fn mock(status: u16, response: String) -> (String, Seen) {
    let seen: Seen = Default::default();
    let app = Router::new()
        .fallback(move |State(seen): State<Seen>, uri: Uri, h: HeaderMap, body: String| {
            let response = response.clone();
            async move {
                seen.lock().unwrap().push((uri.path().to_string(), h, serde_json::from_str(&body).unwrap()));
                (axum::http::StatusCode::from_u16(status).unwrap(), [("content-type", "application/json")], response)
            }
        })
        .with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

fn client(provider: &str, base: &str) -> Llm {
    let key = key_file(0o600).to_str().unwrap().to_string();
    let cfg = LlmConfig::from_vars(vars(&[("LLM_PROVIDER", provider.into()), ("LLM_MODEL", "the-model".into()), ("LLM_API_KEY_FILE", key), ("LLM_BASE_URL", base.into())])).unwrap().unwrap();
    Llm::new(cfg)
}

const SYS: &str = "Be terse.\n\nReply with only a JSON object, no prose or code fences, shaped like: {\"a\":1}";

#[tokio::test]
async fn anthropic_request_and_response_shape() {
    let (base, seen) = mock(200, json!({"content": [{"type": "text", "text": "{\"a\": 7}"}]}).to_string()).await;
    let v = client("anthropic", &base).complete_json("Be terse.", "hello", "{\"a\":1}").await.unwrap();
    assert_eq!(v, json!({"a": 7}));
    let seen = seen.lock().unwrap();
    let (path, h, body) = &seen[0];
    assert_eq!(path, "/v1/messages");
    assert_eq!(h["x-api-key"], KEY);
    assert_eq!(h["anthropic-version"], "2023-06-01");
    assert!(h.get("authorization").is_none());
    assert_eq!(*body, json!({"model": "the-model", "max_tokens": 4096, "system": SYS, "messages": [{"role": "user", "content": "hello"}]}));
}

#[tokio::test]
async fn openai_request_and_response_shape() {
    let (base, seen) = mock(200, json!({"choices": [{"message": {"role": "assistant", "content": "{\"a\": 8}"}}]}).to_string()).await;
    let v = client("openai", &base).complete_json("Be terse.", "hello", "{\"a\":1}").await.unwrap();
    assert_eq!(v, json!({"a": 8}));
    let seen = seen.lock().unwrap();
    let (path, h, body) = &seen[0];
    assert_eq!(path, "/v1/chat/completions");
    assert_eq!(h["authorization"], format!("Bearer {KEY}"));
    assert!(h.get("x-api-key").is_none());
    assert_eq!(
        *body,
        json!({"model": "the-model", "messages": [{"role": "system", "content": SYS}, {"role": "user", "content": "hello"}], "response_format": {"type": "json_object"}})
    );
}

#[tokio::test]
async fn openrouter_uses_the_openai_shape_under_its_api_prefix() {
    // The base override replaces "https://openrouter.ai/api", so the path keeps /v1/chat/completions.
    let (base, seen) = mock(200, json!({"choices": [{"message": {"content": "{\"a\": 9}"}}]}).to_string()).await;
    let v = client("openrouter", &base).complete_json("Be terse.", "hello", "{\"a\":1}").await.unwrap();
    assert_eq!(v, json!({"a": 9}));
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].0, "/v1/chat/completions");
    assert_eq!(seen[0].1["authorization"], format!("Bearer {KEY}"));
    assert_eq!(seen[0].2["response_format"], json!({"type": "json_object"}));
}

#[tokio::test]
async fn fenced_and_chatty_json_is_extracted() {
    for text in ["```json\n{\"a\": 1}\n```", "Sure! Here you go:\n```\n{\"a\": 1}\n```\nEnjoy.", "{\"a\": 1}"] {
        let (base, _) = mock(200, json!({"content": [{"type": "text", "text": text}]}).to_string()).await;
        assert_eq!(client("anthropic", &base).complete_json("s", "u", "h").await.unwrap(), json!({"a": 1}), "{text}");
    }
}

#[tokio::test]
async fn bad_replies_are_errors_without_the_key() {
    let (base, _) = mock(401, format!("{{\"error\": \"bad key {KEY}\"}}")).await;
    let e = client("openai", &base).complete_json("s", "u", "h").await.unwrap_err().to_string();
    assert!(e.contains("401") && !e.contains(KEY), "{e}");

    let (base, _) = mock(200, json!({"content": [{"type": "text", "text": "no json here"}]}).to_string()).await;
    let e = client("anthropic", &base).complete_json("s", "u", "h").await.unwrap_err().to_string();
    assert!(!e.contains(KEY), "{e}");

    let (base, _) = mock(200, "{}".into()).await;
    assert!(client("openai", &base).complete_json("s", "u", "h").await.is_err());

    // Nothing listens: the transport error must not carry the key either.
    let e = client("openai", "http://127.0.0.1:1").complete_json("s", "u", "h").await.unwrap_err().to_string();
    assert!(!e.contains(KEY), "{e}");
}
