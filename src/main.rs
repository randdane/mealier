use std::path::PathBuf;

use mealier::{AuthConfig, Db, ImportService, api};

/// Resolves on ctrl-c or SIGTERM (the container stop signal).
async fn shutdown() {
    let ctrl_c = async { tokio::signal::ctrl_c().await.expect("install ctrl-c handler") };
    let term = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
}

/// `mealier hash-password`: reads a password from stdin, prints its argon2id hash for MEALIER_ADMIN_PASSWORD_FILE.
fn hash_password_command() -> Result<(), Box<dyn std::error::Error>> {
    let mut input = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;
    println!("{}", mealier::hash_password(input.trim_end_matches(['\n', '\r']))?);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    match std::env::args().nth(1).as_deref() {
        Some("hash-password") => return hash_password_command(),
        Some(other) => return Err(format!("unknown command '{other}' (only `hash-password` exists; the server takes no arguments)").into()),
        None => {}
    }
    // Container sets MEALIER_DATA_DIR; locally default to the XDG data dir, never the repo.
    let dir = match std::env::var_os("MEALIER_DATA_DIR") {
        Some(d) => PathBuf::from(d),
        None => {
            let base = match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
                Some(x) => PathBuf::from(x),
                None => PathBuf::from(std::env::var_os("HOME").ok_or("HOME unset")?).join(".local/share"),
            };
            base.join("_r_/mealier")
        }
    };
    std::fs::create_dir_all(&dir)?;
    let db = Db::open(&dir.join("mealier.db")).await?;

    let port = std::env::var("PORT").unwrap_or_else(|_| "9000".into());
    // A container needs 0.0.0.0 so Docker can forward to it; default stays loopback.
    let host = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1".into());
    let listener = tokio::net::TcpListener::bind(format!("{host}:{port}")).await?;
    eprintln!("mealier listening on {}", listener.local_addr()?);
    let import = ImportService::from_env()?;
    let auth = AuthConfig::from_env()?;
    eprintln!(
        "mealier sign-in: oidc {}, local admin {}, cookies {}",
        if auth.oidc.is_some() { "on" } else { "off" },
        if auth.admin_hash.is_some() { "on" } else { "off" },
        if auth.secure_cookies { "secure" } else { "INSECURE (http development only)" },
    );
    if auth.oidc.is_none() && auth.admin_hash.is_none() {
        eprintln!("mealier: no way to sign in is configured (set OIDC_CONFIGURATION_URL or MEALIER_ADMIN_PASSWORD_FILE)");
    }
    let state = api::AppState { db, images_dir: dir.join("images") };
    axum::serve(listener, api::router_with_auth(state, import, auth)).with_graceful_shutdown(shutdown()).await?;
    Ok(())
}
