//! The one way mealier fetches a user-supplied URL (spec, "Fetching").
//! http(s) only; every address is vetted (literal hosts here, DNS names in the
//! resolver, so the connection goes to the vetted IP); redirects are followed by
//! hand so each hop is checked; body and time are bounded.
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::{Client, Url, header};

use crate::{Error, Result};

const MAX_REDIRECTS: usize = 5;
const MAX_BODY: usize = 5 * 1024 * 1024;
const TOTAL_TIMEOUT: Duration = Duration::from_secs(15);

fn public_v4(a: Ipv4Addr) -> bool {
    let o = a.octets();
    !(a.is_loopback()
        || a.is_private()
        || a.is_link_local()
        || a.is_unspecified()
        || a.is_multicast()
        || a.is_broadcast()
        || o[0] == 0
        || o[0] >= 240
        || (o[0] == 100 && o[1] & 0xc0 == 64)) // CGNAT 100.64/10, which is the tailnet
}

/// True for an address on the public internet. The default policy.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => public_v4(a),
        IpAddr::V6(a) => {
            let s = a.segments();
            if let Some(v4) = a.to_ipv4_mapped() {
                return public_v4(v4);
            }
            // NAT64 64:ff9b::/96 embeds an IPv4 address in the last 32 bits.
            if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                return public_v4(Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8));
            }
            !(a.is_loopback() || a.is_unspecified() || a.is_multicast() || a.is_unique_local() || a.is_unicast_link_local() || (s[0] & 0xffc0) == 0xfec0) // fec0::/10, deprecated site-local
        }
    }
}

struct Vet(fn(IpAddr) -> bool);

impl Resolve for Vet {
    fn resolve(&self, name: Name) -> Resolving {
        let allow = self.0;
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), 0)).await?.filter(|a| allow(a.ip())).collect();
            if addrs.is_empty() {
                return Err("host resolves only to refused (non-public) addresses".into());
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

#[derive(Debug)]
pub struct Fetched {
    pub final_url: Url,
    pub body: Vec<u8>,
}

#[derive(Clone)]
pub struct Fetcher {
    client: Client,
    allow: fn(IpAddr) -> bool,
}

impl Default for Fetcher {
    fn default() -> Self {
        Self::with_allow(is_public)
    }
}

impl Fetcher {
    /// Production uses `Fetcher::default()` (public addresses only). The policy
    /// is injectable so tests can reach a server on 127.0.0.1; nothing in config or env can.
    #[doc(hidden)]
    pub fn with_allow(allow: fn(IpAddr) -> bool) -> Fetcher {
        let client = Client::builder()
            .dns_resolver(Vet(allow))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(TOTAL_TIMEOUT)
            .user_agent("mealier")
            .build()
            .expect("build http client");
        Fetcher { client, allow }
    }

    pub async fn get(&self, url: &str) -> Result<Fetched> {
        tokio::time::timeout(TOTAL_TIMEOUT, self.get_inner(url)).await.map_err(|_| bad("timed out"))?
    }

    async fn get_inner(&self, url: &str) -> Result<Fetched> {
        let mut url = Url::parse(url.trim()).map_err(|e| bad(format!("not a valid URL: {e}")))?;
        for _ in 0..=MAX_REDIRECTS {
            self.check(&url)?;
            let mut resp = self.client.get(url.clone()).send().await.map_err(|e| bad(format!("request failed: {}", root(&e))))?;
            if resp.status().is_redirection() {
                let loc = resp.headers().get(header::LOCATION).and_then(|v| v.to_str().ok()).ok_or_else(|| bad("redirect without a Location"))?;
                url = url.join(loc).map_err(|e| bad(format!("bad redirect: {e}")))?;
                continue;
            }
            if !resp.status().is_success() {
                return Err(bad(format!("server answered {}", resp.status())));
            }
            let mut body = Vec::new();
            while let Some(chunk) = resp.chunk().await.map_err(|e| bad(format!("read failed: {}", root(&e))))? {
                if body.len() + chunk.len() > MAX_BODY {
                    return Err(bad("response is over 5 MB"));
                }
                body.extend_from_slice(&chunk);
            }
            return Ok(Fetched { final_url: url, body });
        }
        Err(bad("too many redirects"))
    }

    /// Scheme and literal-IP check. DNS names are vetted by the resolver at connect time.
    fn check(&self, url: &Url) -> Result<()> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(bad("only http and https URLs are allowed"));
        }
        let ip = match url.host() {
            Some(url::Host::Ipv4(a)) => Some(IpAddr::V4(a)),
            Some(url::Host::Ipv6(a)) => Some(IpAddr::V6(a)),
            Some(url::Host::Domain(_)) => None,
            None => return Err(bad("URL has no host")),
        };
        if ip.is_some_and(|ip| !(self.allow)(ip)) {
            return Err(bad("address is not allowed (private, loopback or internal)"));
        }
        Ok(())
    }
}

fn bad(m: impl Into<String>) -> Error {
    Error::Validation(m.into())
}

/// reqwest wraps the interesting error (our resolver's refusal) several layers down.
fn root(e: &reqwest::Error) -> String {
    let mut s: &dyn std::error::Error = e;
    while let Some(n) = s.source() {
        s = n;
    }
    s.to_string()
}

