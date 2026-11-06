use std::path::PathBuf;

use mealier::{Db, api};

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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
    axum::serve(listener, api::router(db)).with_graceful_shutdown(shutdown()).await?;
    Ok(())
}
