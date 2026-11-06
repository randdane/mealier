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

/// `mealier healthcheck`: GET /healthz on loopback with a plain socket (the image has no shell or curl); exit 0 only on a 200.
fn healthcheck_command() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{Read, Write};
    use std::time::Duration;
    let port = std::env::var("PORT").unwrap_or_else(|_| "9000".into());
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse()?;
    let t = Duration::from_secs(3);
    let mut s = std::net::TcpStream::connect_timeout(&addr, t)?;
    s.set_read_timeout(Some(t))?;
    s.set_write_timeout(Some(t))?;
    s.write_all(b"GET /healthz HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")?;
    let mut head = [0u8; 12];
    s.read_exact(&mut head)?;
    if &head == b"HTTP/1.0 200" || &head == b"HTTP/1.1 200" {
        Ok(())
    } else {
        Err(format!("unhealthy: {}", String::from_utf8_lossy(&head)).into())
    }
}

/// Container sets MEALIER_DATA_DIR; locally default to the XDG data dir, never the repo.
fn data_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(match std::env::var_os("MEALIER_DATA_DIR") {
        Some(d) => PathBuf::from(d),
        None => {
            let base = match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
                Some(x) => PathBuf::from(x),
                None => PathBuf::from(std::env::var_os("HOME").ok_or("HOME unset")?).join(".local/share"),
            };
            base.join("_r_/mealier")
        }
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    match std::env::args().nth(1).as_deref() {
        Some("hash-password") => return hash_password_command(),
        Some("healthcheck") => {
            return healthcheck_command().map_err(|e| {
                eprintln!("healthcheck: {e}");
                std::process::exit(1)
            });
        }
        Some("import-mealie") => {
            let zip = std::env::args().nth(2).ok_or("usage: mealier import-mealie <backup.zip>")?;
            let dir = data_dir()?;
            std::fs::create_dir_all(&dir)?;
            let db = Db::open(&dir.join("mealier.db")).await?;
            let report = mealier::import_mealie_backup(&db, std::path::Path::new(&zip), &dir.join("images")).await?;
            println!("{report:#?}");
            return Ok(());
        }
        Some("run-scheduled") => {
            let db = Db::open(&data_dir()?.join(mealier::backup::DB_FILE)).await?;
            let r = db.run_scheduled(&mealier::Clock::now()).await?;
            println!("scheduled webhooks: sent {}, failed {}, not due {}, already sent {}, nothing planned {}", r.sent, r.failed, r.not_due, r.already_sent, r.empty);
            if r.failed > 0 {
                std::process::exit(1);
            }
            return Ok(());
        }
        Some("export") => {
            let out = std::env::args().nth(2).ok_or("usage: mealier export <out.zip>")?;
            mealier::backup::export_dir(&data_dir()?, std::path::Path::new(&out)).await?;
            println!("exported to {out}");
            return Ok(());
        }
        Some("restore") => {
            let zip = std::env::args().nth(2).ok_or("usage: mealier restore <in.zip> (stop the server first)")?;
            let restored = mealier::backup::restore_offline(std::path::Path::new(&zip), &data_dir()?).await?;
            match restored.previous {
                Some(p) => println!("restored; the data it replaced is in {}", p.display()),
                None => println!("restored"),
            }
            return Ok(());
        }
        Some(other) => return Err(format!("unknown command '{other}' (commands: hash-password, healthcheck, import-mealie <zip>, run-scheduled, export <out.zip>, restore <in.zip>; the server takes no arguments)").into()),
        None => {}
    }
    let dir = data_dir()?;
    std::fs::create_dir_all(&dir)?;
    // Held until the process exits: `mealier restore` refuses to touch a directory a server is using.
    let _lock = mealier::backup::lock_data_dir(&dir)?;
    let db = Db::open(&dir.join(mealier::backup::DB_FILE)).await?;

    let port = std::env::var("PORT").unwrap_or_else(|_| "9000".into());
    // A container needs 0.0.0.0 so Docker can forward to it; default stays loopback.
    let host = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1".into());
    let listener = tokio::net::TcpListener::bind(format!("{host}:{port}")).await?;
    eprintln!("mealier listening on {}", listener.local_addr()?);
    let import = ImportService::from_env()?;
    match import.llm() {
        Some(l) => eprintln!("mealier llm: {:?} {}", l.provider(), l.model()),
        None => eprintln!("mealier llm: off"),
    }
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
    // A restore from the UI swaps the files under the open pool: after answering, the process exits and the container restarts it.
    let restart = std::sync::Arc::new(mealier::backup::Restart::new());
    let stop = {
        let restart = restart.clone();
        async move {
            tokio::select! {
                _ = shutdown() => {}
                _ = restart.requested() => eprintln!("mealier: restore done, exiting so the container restarts"),
            }
        }
    };
    axum::serve(listener, api::router_with_restart(state, import, auth, restart)).with_graceful_shutdown(stop).await?;
    Ok(())
}
