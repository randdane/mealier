//! Auth settings from the environment (the closure stands in for it).
use std::collections::HashMap;

use mealier::{AuthConfig, Error, hash_password};

fn cfg(vars: &[(&str, &str)]) -> Result<AuthConfig, Error> {
    let m: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    AuthConfig::from_vars(|k| m.get(k).cloned())
}

fn file(dir: &tempfile::TempDir, name: &str, content: &str) -> String {
    let p = dir.path().join(name);
    std::fs::write(&p, content).unwrap();
    p.to_str().unwrap().to_string()
}

#[test]
fn nothing_set_means_secure_cookies_and_no_login_methods() {
    let c = cfg(&[]).unwrap();
    assert!(c.secure_cookies && c.base_url.is_none() && c.admin_hash.is_none() && c.oidc.is_none());
}

#[test]
fn only_the_value_1_turns_secure_cookies_off() {
    assert!(!cfg(&[("MEALIER_INSECURE_COOKIES", "1")]).unwrap().secure_cookies);
    for v in ["0", "true", "yes", ""] {
        assert!(cfg(&[("MEALIER_INSECURE_COOKIES", v)]).unwrap().secure_cookies, "{v:?}");
    }
}

#[test]
fn the_base_url_must_be_an_http_url() {
    assert_eq!(cfg(&[("MEALIER_BASE_URL", "https://mealier.example")]).unwrap().base_url.as_deref(), Some("https://mealier.example"));
    for bad in ["mealier.example", "ftp://x.example", "javascript:alert(1)"] {
        assert!(matches!(cfg(&[("MEALIER_BASE_URL", bad)]), Err(Error::Validation(_))), "{bad}");
    }
}

#[test]
fn the_admin_password_file_holds_an_argon2id_hash_that_is_trimmed() {
    let dir = tempfile::tempdir().unwrap();
    let h = hash_password("a long enough password").unwrap();
    let f = file(&dir, "hash", &format!("{h}\n"));
    assert_eq!(cfg(&[("MEALIER_ADMIN_PASSWORD_FILE", &f)]).unwrap().admin_hash.as_deref(), Some(h.as_str()));

    let junk = file(&dir, "junk", "hunter2-not-a-hash\n");
    let err = cfg(&[("MEALIER_ADMIN_PASSWORD_FILE", &junk)]).err().unwrap();
    assert!(matches!(err, Error::Validation(_)));
    assert!(!err.to_string().contains("hunter2"), "file contents must not leak: {err}");
    assert!(cfg(&[("MEALIER_ADMIN_PASSWORD_FILE", "/nonexistent/hash")]).is_err(), "a named but unreadable file is an error, not 'disabled'");
}

#[test]
fn oidc_is_on_only_when_the_configuration_url_is_set_and_then_needs_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let secret = file(&dir, "secret", "s3cr3t-value\n");
    let full = [
        ("OIDC_CONFIGURATION_URL", "https://auth.example/application/o/mealier/.well-known/openid-configuration"),
        ("OIDC_CLIENT_ID", "mealier"),
        ("OIDC_CLIENT_SECRET_FILE", secret.as_str()),
        ("MEALIER_BASE_URL", "https://mealier.example"),
    ];
    let o = cfg(&full).unwrap().oidc.unwrap();
    assert_eq!((o.client_id.as_str(), o.client_secret.as_str()), ("mealier", "s3cr3t-value"));
    assert_eq!((o.user_group.as_str(), o.admin_group.as_str()), ("mealier-users", "mealier-admins"));

    let mut with_groups = full.to_vec();
    with_groups.extend([("OIDC_USER_GROUP", "eaters"), ("OIDC_ADMIN_GROUP", "chefs")]);
    let o = cfg(&with_groups).unwrap().oidc.unwrap();
    assert_eq!((o.user_group.as_str(), o.admin_group.as_str()), ("eaters", "chefs"));

    for missing in ["OIDC_CLIENT_ID", "OIDC_CLIENT_SECRET_FILE", "MEALIER_BASE_URL"] {
        let partial: Vec<_> = full.iter().filter(|(k, _)| *k != missing).cloned().collect();
        let err = cfg(&partial).err().unwrap_or_else(|| panic!("{missing} is required")).to_string();
        assert!(err.contains(missing), "{err}");
    }
    let empty = file(&dir, "empty", "\n");
    let mut with_empty = full.to_vec();
    with_empty.retain(|(k, _)| *k != "OIDC_CLIENT_SECRET_FILE");
    with_empty.push(("OIDC_CLIENT_SECRET_FILE", empty.as_str()));
    assert!(cfg(&with_empty).is_err(), "an empty secret file is an error");

    assert!(cfg(&[("OIDC_CLIENT_ID", "mealier")]).unwrap().oidc.is_none(), "no configuration URL, no OIDC");
}
