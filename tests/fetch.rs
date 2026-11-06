use std::net::IpAddr;

use axum::Router;
use axum::response::Redirect;
use axum::routing::get;
use mealier::fetch::{Fetcher, is_public};

#[test]
fn ip_policy_table() {
    let refused = [
        "127.0.0.1", "10.1.2.3", "172.20.0.1", "192.168.40.7", "100.100.3.4", "169.254.169.254", "0.0.0.0", "224.0.0.1",
        "::1", "::", "fd00::1", "fe80::1", "ff02::1", "::ffff:192.168.1.1", "::ffff:127.0.0.1", "::ffff:100.100.3.4", "64:ff9b::a00:1",
    ];
    for a in refused {
        assert!(!is_public(a.parse::<IpAddr>().unwrap()), "{a} should be refused");
    }
    for a in ["93.184.216.34", "2606:4700::1", "172.32.0.1", "100.128.0.1", "::ffff:93.184.216.34"] {
        assert!(is_public(a.parse::<IpAddr>().unwrap()), "{a} should be allowed");
    }
}

async fn serve(app: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

fn loopback_only(ip: IpAddr) -> bool {
    ip == IpAddr::from([127, 0, 0, 1])
}

#[tokio::test]
async fn follows_a_redirect_to_an_allowed_host_and_reports_the_final_url() {
    let base = serve(Router::new().route("/a", get(|| async { Redirect::temporary("/b") })).route("/b", get(|| async { "hello" }))).await;
    let f = Fetcher::with_allow(loopback_only).get(&format!("{base}/a")).await.unwrap();
    assert_eq!(f.final_url.as_str(), format!("{base}/b"));
    assert_eq!(f.body, b"hello");
}

#[tokio::test]
async fn a_redirect_to_a_refused_host_fails() {
    let base = serve(Router::new().route("/a", get(|| async { Redirect::temporary("http://10.0.0.1/secret") }))).await;
    let err = Fetcher::with_allow(loopback_only).get(&format!("{base}/a")).await.unwrap_err();
    assert_eq!(err.to_string(), "address is not allowed (private, loopback or internal)");
}

#[tokio::test]
async fn default_policy_refuses_literal_and_non_http_urls() {
    let f = Fetcher::default();
    for (url, msg) in [
        ("http://127.0.0.1:1/", "address is not allowed (private, loopback or internal)"),
        ("http://[::1]/", "address is not allowed (private, loopback or internal)"),
        ("http://2130706433/", "address is not allowed (private, loopback or internal)"),
        ("file:///etc/passwd", "only http and https URLs are allowed"),
    ] {
        assert_eq!(f.get(url).await.unwrap_err().to_string(), msg, "{url}");
    }
}

#[tokio::test]
async fn a_name_resolving_to_loopback_is_refused_by_the_resolver() {
    let err = Fetcher::default().get("http://localhost:1/").await.unwrap_err().to_string();
    assert!(err.contains("resolves only to refused"), "{err}");
}

#[tokio::test]
async fn stops_after_five_redirects_and_caps_the_body() {
    let big = || async { vec![b'x'; 5 * 1024 * 1024 + 1] };
    let base = serve(Router::new().route("/loop", get(|| async { Redirect::temporary("/loop") })).route("/big", get(big))).await;
    let f = Fetcher::with_allow(loopback_only);
    assert_eq!(f.get(&format!("{base}/loop")).await.unwrap_err().to_string(), "too many redirects");
    assert_eq!(f.get(&format!("{base}/big")).await.unwrap_err().to_string(), "response is over 5 MB");
}
