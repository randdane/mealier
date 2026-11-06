use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn healthcheck(port: u16) -> bool {
    Command::new(env!("CARGO_BIN_EXE_mealier")).arg("healthcheck").env("PORT", port.to_string()).stderr(Stdio::null()).status().unwrap().success()
}

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

#[test]
fn healthcheck_passes_against_running_server_and_fails_on_closed_port() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let _server = Kill(
        Command::new(env!("CARGO_BIN_EXE_mealier"))
            .env("PORT", port.to_string())
            .env("MEALIER_DATA_DIR", dir.path())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while !healthcheck(port) {
        assert!(Instant::now() < deadline, "server never became healthy");
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(_server);
    assert!(!healthcheck(free_port()), "closed port must fail");
}
