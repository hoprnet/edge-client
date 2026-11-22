//! Lets the embedder decide, at run time, where the Curvy relayer's traffic goes.
//!
//! A VPN client arms a killswitch before its tunnel exists, and its exit holds the tunnel's traffic back
//! until the first PIX deposit is seen. A deposit that has to reach the relayer through that same tunnel
//! therefore cannot be made exactly when it is needed. The embedder can instead open a separate, unpaid
//! HOPR session to the relayer (a local TCP listener that forwards to `host:port` through the exit) and
//! point this route at the listener. TLS still runs end to end with the relayer; only the TCP connection
//! is made to the listener instead of the relayer's own address.
//!
//! While no route is set, the relayer is reached directly as before.
//!
//! Setting a route also opens the first connection through it in the background. A HOPR session listener
//! opens its session only when a connection arrives, so without this the payment that needs the relayer
//! first would wait for the session and the TLS handshake.

use std::{
    net::SocketAddr,
    sync::{Arc, OnceLock, RwLock},
    time::Duration,
};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// Shared switch for the relayer's route. Cloning shares the same route.
#[derive(Clone, Debug)]
pub struct RelayerRoute {
    origin: Arc<str>,
    host: Arc<str>,
    port: u16,
    via: Arc<RwLock<Option<SocketAddr>>>,
    http: Arc<OnceLock<reqwest::Client>>,
}

/// How long the connection opened ahead of the first relayer request may take.
const WARM_UP_TIMEOUT: Duration = Duration::from_secs(20);

impl RelayerRoute {
    /// A route for the relayer at `url`; `None` when the URL has no host.
    pub fn for_url(url: &url::Url) -> Option<Self> {
        Some(Self {
            origin: Arc::from(url.origin().ascii_serialization()),
            host: Arc::from(url.host_str()?.to_ascii_lowercase()),
            port: url.port_or_known_default()?,
            via: Arc::default(),
            http: Arc::default(),
        })
    }

    /// The relayer's host name, as a session target would name it.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The relayer's TCP port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Sends new relayer connections to `listener` (for example a HOPR session's local TCP listener) and
    /// opens the first one in the background, so that it is ready when the pool needs the relayer.
    pub fn set(&self, listener: SocketAddr) {
        tracing::info!(host = %self.host, %listener, "relayer traffic now goes through a separate session");
        *self
            .via
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(listener);
        self.warm_up();
    }

    /// Opens a connection to the relayer and leaves it in the HTTP client's pool. Any answer will do.
    fn warm_up(&self) {
        let (Some(http), Ok(runtime)) = (
            self.http.get().cloned(),
            tokio::runtime::Handle::try_current(),
        ) else {
            return;
        };
        let origin = self.origin.clone();
        runtime.spawn(async move {
            match http.head(origin.as_ref()).timeout(WARM_UP_TIMEOUT).send().await {
                Ok(_) => tracing::debug!(%origin, "relayer connection opened ahead of the first request"),
                Err(error) => tracing::debug!(%origin, %error, "could not open the relayer connection ahead of time"),
            }
        });
    }

    /// Back to reaching the relayer directly.
    pub fn clear(&self) {
        let previous = self
            .via
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if previous.is_some() {
            tracing::info!(host = %self.host, "relayer traffic goes direct again");
        }
    }

    /// Where relayer connections go right now; `None` means direct.
    pub fn current(&self) -> Option<SocketAddr> {
        *self
            .via
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn resolver(&self) -> RouteResolver {
        RouteResolver {
            host: self.host.clone(),
            via: self.via.clone(),
        }
    }

    /// An HTTP client that follows this route for the relayer's host and resolves every other name normally.
    pub(crate) fn http_client(&self) -> reqwest::Result<reqwest::Client> {
        let http = reqwest::Client::builder()
            .dns_resolver(Arc::new(self.resolver()))
            // A pooled connection belongs to the route it was opened on; do not keep one across a change for long.
            .pool_idle_timeout(Duration::from_secs(10))
            .build()?;
        Ok(self.http.get_or_init(|| http).clone())
    }
}

/// The name lookup of the HTTP client. It shares the route's switch but not the route itself, which
/// holds the HTTP client.
struct RouteResolver {
    host: Arc<str>,
    via: Arc<RwLock<Option<SocketAddr>>>,
}

impl Resolve for RouteResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = self.host.clone();
        let via = *self
            .via
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Box::pin(async move {
            if name.as_str().eq_ignore_ascii_case(&host)
                && let Some(listener) = via
            {
                // The URL names no port, so the connector keeps the port given here (hyper-util only fills in port 0).
                return Ok(Box::new(std::iter::once(listener)) as Addrs);
            }
            let found: Vec<SocketAddr> =
                tokio::net::lookup_host((name.as_str(), 0)).await?.collect();
            Ok(Box::new(found.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(text: &str) -> Name {
        text.parse().expect("a host name")
    }

    #[tokio::test]
    async fn relayer_host_follows_the_route_and_other_hosts_do_not() -> anyhow::Result<()> {
        let route = RelayerRoute::for_url(&"https://api.curvy.box".parse()?).expect("has a host");
        assert_eq!((route.host(), route.port()), ("api.curvy.box", 443));
        let listener: SocketAddr = "127.0.0.1:45001".parse()?;
        route.set(listener);
        let resolver = route.resolver();

        let via: Vec<_> = resolver
            .resolve(name("API.curvy.box"))
            .await
            .map_err(|e| anyhow::anyhow!(e))?
            .collect();
        assert_eq!(via, vec![listener]);

        let other: Vec<_> = resolver
            .resolve(name("localhost"))
            .await
            .map_err(|e| anyhow::anyhow!(e))?
            .collect();
        assert!(!other.is_empty() && !other.contains(&listener));
        Ok(())
    }

    #[tokio::test]
    async fn a_cleared_route_resolves_the_relayer_normally() -> anyhow::Result<()> {
        // `localhost` stands in for the relayer so the test needs no network.
        let route = RelayerRoute::for_url(&"https://localhost".parse()?).expect("has a host");
        let listener: SocketAddr = "127.0.0.1:45001".parse()?;
        route.set(listener);
        route.clear();
        assert_eq!(route.current(), None);
        let found: Vec<_> = route
            .resolver()
            .resolve(name("localhost"))
            .await
            .map_err(|e| anyhow::anyhow!(e))?
            .collect();
        assert!(!found.is_empty() && !found.contains(&listener));
        Ok(())
    }
}
