//! The Blokli service endpoint and its DNS-bypass configuration.
//!
//! This module is the only place in the crate that constructs a
//! [`HoprBlokliClientConfig`] or calls [`create_blokli_client`]. Every path that needs a
//! Blokli client resolves through [`BlokliEndpoint::build_client`], so a caller-supplied
//! DNS override cannot be dropped on the way to the client.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;

use hopr_chain_connector::{
    HoprBlokliClientConfig, blokli_client::BlokliClient, create_blokli_client,
};
use url::Url;

use crate::blokli::DEFAULT_BLOKLI_URL;
use crate::errors::EdgliError;

/// DNS resolution override for the Blokli endpoint host.
///
/// Pins the endpoint hostname to a fixed address so no system DNS lookup is needed.
/// The request URL is not rewritten: the HTTP `Host` header, TLS SNI and certificate
/// validation still use the original hostname.
///
/// When [`Self::port`] is `None`, the endpoint URL's port (or its scheme default) is used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlokliDnsOverride {
    /// IP address to connect to instead of resolving the endpoint host.
    pub ip: IpAddr,
    /// Optional port override.
    pub port: Option<u16>,
}

impl BlokliDnsOverride {
    /// Creates an override for `ip`, optionally pinning the port as well.
    pub const fn new(ip: IpAddr, port: Option<u16>) -> Self {
        Self { ip, port }
    }
}

impl FromStr for BlokliDnsOverride {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if let Ok(ip) = value.parse::<IpAddr>() {
            return Ok(Self { ip, port: None });
        }

        if let Ok(addr) = value.parse::<SocketAddr>() {
            return Ok(Self {
                ip: addr.ip(),
                port: Some(addr.port()),
            });
        }

        Err(format!(
            "invalid DNS override '{value}', expected <IP_ADDRESS> or <IP_ADDRESS>:<PORT>"
        ))
    }
}

impl fmt::Display for BlokliDnsOverride {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.port {
            Some(port) => write!(f, "{}", SocketAddr::new(self.ip, port)),
            None => write!(f, "{}", self.ip),
        }
    }
}

/// The Blokli service endpoint: a URL plus an optional DNS-resolution override.
///
/// [`Default`] yields [`DEFAULT_BLOKLI_URL`] resolved through system DNS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlokliEndpoint {
    /// URL of the Blokli service.
    pub url: Url,
    /// When set, bypasses system DNS for [`Self::url`]'s host.
    pub dns_override: Option<BlokliDnsOverride>,
}

impl Default for BlokliEndpoint {
    fn default() -> Self {
        Self {
            url: DEFAULT_BLOKLI_URL.clone(),
            dns_override: None,
        }
    }
}

impl BlokliEndpoint {
    /// Creates an endpoint for `url` resolved through system DNS.
    pub fn new(url: Url) -> Self {
        Self {
            url,
            dns_override: None,
        }
    }

    /// Sets the DNS override, replacing any previously configured one.
    pub fn with_dns_override(mut self, dns_override: Option<BlokliDnsOverride>) -> Self {
        self.dns_override = dns_override;
        self
    }

    /// Creates an endpoint from a URL string, falling back to [`DEFAULT_BLOKLI_URL`]
    /// when `url` is `None`.
    pub fn parse(
        url: Option<&str>,
        dns_override: Option<BlokliDnsOverride>,
    ) -> Result<Self, EdgliError> {
        let url = match url {
            Some(url) => url
                .parse()
                .map_err(|e| EdgliError::ConfigError(format!("invalid Blokli URL '{url}': {e}")))?,
            None => DEFAULT_BLOKLI_URL.clone(),
        };

        Ok(Self { url, dns_override })
    }

    /// Converts the endpoint into the chain connector's client configuration.
    ///
    /// The connector represents the override as a tuple, so this is where
    /// [`BlokliDnsOverride`] is destructured.
    pub(crate) fn to_client_config(&self) -> HoprBlokliClientConfig {
        HoprBlokliClientConfig {
            url: self.url.clone(),
            dns_override: self.dns_override.map(|o| (o.ip, o.port)),
        }
    }

    /// Builds a Blokli client for this endpoint.
    pub(crate) fn build_client(&self) -> BlokliClient {
        create_blokli_client(self.to_client_config())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn parse_defaults_to_production_url() {
        let endpoint = BlokliEndpoint::parse(None, None).unwrap();
        assert_eq!(endpoint.url, *DEFAULT_BLOKLI_URL);
        assert_eq!(endpoint.dns_override, None);
    }

    #[test]
    fn parse_keeps_dns_override() {
        let dns_override = Some(BlokliDnsOverride::new(v4(10, 1, 2, 1), Some(3002)));
        let endpoint =
            BlokliEndpoint::parse(Some("https://blokli.example.com"), dns_override).unwrap();
        assert_eq!(endpoint.url.as_str(), "https://blokli.example.com/");
        assert_eq!(endpoint.dns_override, dns_override);
    }

    #[test]
    fn parse_rejects_invalid_url() {
        let error = BlokliEndpoint::parse(Some("not a url"), None).unwrap_err();
        assert_eq!(
            error.to_string(),
            "configuration error: 'invalid Blokli URL 'not a url': relative URL without a base'"
        );
    }

    #[test]
    fn default_endpoint_matches_default_url() {
        let endpoint = BlokliEndpoint::default();
        assert_eq!(endpoint.url, *DEFAULT_BLOKLI_URL);
        assert_eq!(endpoint.dns_override, None);
    }

    /// Regression guard: the safeless/onboarding path used to build its client through
    /// `HoprBlokliClientConfig::new`, which hardcodes `dns_override: None`, so a
    /// caller-supplied DNS bypass was silently discarded. Both the node path and the
    /// safeless path now resolve through this single seam.
    #[test]
    fn to_client_config_propagates_dns_override() {
        let dns_override = BlokliDnsOverride::new(v4(10, 1, 2, 1), Some(3002));
        let endpoint = BlokliEndpoint::default().with_dns_override(Some(dns_override));

        let config = endpoint.to_client_config();
        assert_eq!(config.url, endpoint.url);
        assert_eq!(config.dns_override, Some((v4(10, 1, 2, 1), Some(3002))));
    }

    #[test]
    fn to_client_config_without_override_uses_system_dns() {
        let config = BlokliEndpoint::default().to_client_config();
        assert_eq!(config.dns_override, None);
    }

    #[test]
    fn from_str_parses_bare_ip() {
        let parsed: BlokliDnsOverride = "127.0.0.1".parse().unwrap();
        assert_eq!(parsed, BlokliDnsOverride::new(v4(127, 0, 0, 1), None));
    }

    #[test]
    fn from_str_parses_ip_and_port() {
        let parsed: BlokliDnsOverride = "10.1.2.1:3002".parse().unwrap();
        assert_eq!(parsed, BlokliDnsOverride::new(v4(10, 1, 2, 1), Some(3002)));
    }

    #[test]
    fn from_str_parses_ipv6_forms() {
        let loopback = IpAddr::V6(Ipv6Addr::LOCALHOST);

        let bare: BlokliDnsOverride = "::1".parse().unwrap();
        assert_eq!(bare, BlokliDnsOverride::new(loopback, None));

        let with_port: BlokliDnsOverride = "[::1]:3002".parse().unwrap();
        assert_eq!(with_port, BlokliDnsOverride::new(loopback, Some(3002)));
    }

    #[test]
    fn from_str_rejects_garbage() {
        let error = "nope".parse::<BlokliDnsOverride>().unwrap_err();
        assert_eq!(
            error,
            "invalid DNS override 'nope', expected <IP_ADDRESS> or <IP_ADDRESS>:<PORT>"
        );
    }

    #[test]
    fn display_from_str_roundtrip() {
        for input in ["127.0.0.1", "10.1.2.1:3002", "::1", "[::1]:3002"] {
            let parsed: BlokliDnsOverride = input.parse().unwrap();
            let rendered = parsed.to_string();
            assert_eq!(rendered.parse::<BlokliDnsOverride>().unwrap(), parsed);
        }
    }
}
