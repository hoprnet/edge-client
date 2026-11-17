use std::error::Error;

use blokli_client::errors::{BlokliClientError, ErrorKind as BlokliErrorKind};
use cynic::http::CynicReqwestError;
use hopr_chain_connector::errors::ConnectorError;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum EdgliError {
    #[error("file error: '{0}'")]
    FileError(String),

    #[error("configuration error: '{0}'")]
    ConfigError(String),

    #[error("serialization failed: '{0}'")]
    SerializationError(String),

    #[error("validation failed: '{0}'")]
    ValidationError(String),

    #[error(transparent)]
    HoprLibError(#[from] hopr_lib::errors::HoprLibError),

    #[error("os error: '{0}'")]
    OsError(String),

    #[error(transparent)]
    Connector(#[from] ConnectorError),

    #[error(transparent)]
    BlokliClient(#[from] BlokliClientError),

    #[error(transparent)]
    Strategy(#[from] hopr_strategy::errors::StrategyError),

    #[error(transparent)]
    TicketManager(#[from] hopr_ticket_manager::TicketManagerError),

    #[error(transparent)]
    General(#[from] hopr_lib::api::types::primitive::errors::GeneralError),

    /// An upstream validator rejected a setting; the source keeps its own error type.
    #[error("invalid {setting}: {source}")]
    InvalidSetting {
        setting: &'static str,
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },

    #[cfg(any(feature = "runtime-tokio", feature = "blokli"))]
    #[error("background task failed: {0}")]
    TaskFailed(#[from] tokio::task::JoinError),

    #[error("exit-node discovery stopped")]
    DiscoveryStopped,
}

pub type Result<T> = std::result::Result<T, EdgliError>;

/// What the caller can tell the user about a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// DNS, connect or TLS failed: the user's network, proxy, firewall or captive portal.
    Unreachable,
    /// Blokli answered but is unhealthy, overloaded or too slow.
    ServiceUnavailable,
    /// Blokli's answer could not be understood, so this client and Blokli disagree on the protocol.
    Incompatible,
    Other,
}

impl EdgliError {
    /// Classifies the failure by walking its source chain down to the first cause it recognises.
    pub fn failure_kind(&self) -> FailureKind {
        // `#[error(transparent)]` hides the wrapped error from `source()`, so start at it directly.
        let mut next: Option<&(dyn Error + 'static)> = match self {
            EdgliError::Connector(error) => Some(error),
            EdgliError::BlokliClient(error) => Some(error),
            EdgliError::HoprLibError(error) => Some(error),
            other => Some(other),
        };
        while let Some(error) = next {
            if let Some(kind) = classify(error) {
                return kind;
            }
            next = error.source();
        }
        FailureKind::Other
    }
}

/// `None` means "not decided here", so the caller keeps walking the source chain.
fn classify(error: &(dyn Error + 'static)) -> Option<FailureKind> {
    if let Some(connector) = error.downcast_ref::<ConnectorError>() {
        return classify_connector(connector);
    }
    if let Some(kind) = error.downcast_ref::<BlokliErrorKind>() {
        return classify_blokli(kind);
    }
    if let Some(cynic) = error.downcast_ref::<CynicReqwestError>() {
        return classify_cynic(cynic);
    }
    if let Some(reqwest) = error.downcast_ref::<reqwest::Error>() {
        return classify_reqwest(reqwest);
    }
    if let Some(inner) = error
        .downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::get_ref)
    {
        // `io::Error::source()` skips the error it wraps, so look at that error here.
        return classify(inner);
    }
    if error.is::<rustls::Error>() {
        return Some(FailureKind::Unreachable);
    }
    None
}

fn classify_connector(error: &ConnectorError) -> Option<FailureKind> {
    match error {
        ConnectorError::ServerNotHealthy | ConnectorError::ConnectionTimeout => {
            Some(FailureKind::ServiceUnavailable)
        }
        ConnectorError::CacheError(inner) => classify_connector(inner),
        ConnectorError::ClientError(client) => classify_blokli(client.kind()),
        _ => None,
    }
}

fn classify_blokli(kind: &BlokliErrorKind) -> Option<FailureKind> {
    match kind {
        BlokliErrorKind::Timeout | BlokliErrorKind::HoprActionThrottled { .. } => {
            Some(FailureKind::ServiceUnavailable)
        }
        BlokliErrorKind::BlokliError {
            kind: "overloaded", ..
        } => Some(FailureKind::ServiceUnavailable),
        BlokliErrorKind::ParseError
        | BlokliErrorKind::GraphQLError(_)
        | BlokliErrorKind::NoData
        | BlokliErrorKind::Serialization(_) => Some(FailureKind::Incompatible),
        BlokliErrorKind::Reqwest(reqwest) => classify_reqwest(reqwest),
        BlokliErrorKind::Cynic(cynic) => classify_cynic(cynic),
        _ => None,
    }
}

fn classify_cynic(error: &CynicReqwestError) -> Option<FailureKind> {
    match error {
        CynicReqwestError::ReqwestError(reqwest) => classify_reqwest(reqwest),
        CynicReqwestError::ErrorResponse(status, _) => classify_status(*status),
    }
}

fn classify_reqwest(error: &reqwest::Error) -> Option<FailureKind> {
    if error.is_connect() {
        return Some(FailureKind::Unreachable);
    }
    if error.is_timeout() {
        return Some(FailureKind::ServiceUnavailable);
    }
    error.status().and_then(classify_status)
}

fn classify_status(status: reqwest::StatusCode) -> Option<FailureKind> {
    let is_overloaded =
        status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
    is_overloaded.then_some(FailureKind::ServiceUnavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_error_display() {
        let e = EdgliError::FileError("not found".into());
        assert_eq!(e.to_string(), "file error: 'not found'");
    }

    #[test]
    fn config_error_display() {
        let e = EdgliError::ConfigError("bad value".into());
        assert_eq!(e.to_string(), "configuration error: 'bad value'");
    }

    #[test]
    fn serialization_error_display() {
        let e = EdgliError::SerializationError("parse failed".into());
        assert_eq!(e.to_string(), "serialization failed: 'parse failed'");
    }

    #[test]
    fn validation_error_display() {
        let e = EdgliError::ValidationError("out of range".into());
        assert_eq!(e.to_string(), "validation failed: 'out of range'");
    }

    #[test]
    fn os_error_display() {
        let e = EdgliError::OsError("signal failed".into());
        assert_eq!(e.to_string(), "os error: 'signal failed'");
    }

    #[test]
    fn hopr_lib_error_converts_via_from() {
        let hopr_err = hopr_lib::errors::HoprLibError::GeneralError("something broke".into());
        let edgli_err = EdgliError::from(hopr_err);
        assert!(
            edgli_err.to_string().contains("something broke"),
            "transparent display must include underlying message"
        );
    }

    #[test]
    fn result_alias_is_edgli_error() {
        let r: Result<()> = Err(EdgliError::ConfigError("x".into()));
        assert!(r.is_err());
    }

    mod failure_kind {
        use std::sync::Arc;

        use hopr_lib::errors::HoprLibError;

        use super::*;

        fn client_error(kind: BlokliErrorKind) -> EdgliError {
            EdgliError::Connector(ConnectorError::ClientError(kind.into()))
        }

        /// A real `reqwest::Error` without a network: nothing listens on port 1 of the loopback.
        async fn connection_refused() -> reqwest::Error {
            reqwest::Client::new()
                .get("http://127.0.0.1:1/")
                .send()
                .await
                .expect_err("nothing listens on port 1")
        }

        /// A real `reqwest::Error` without a network: a local listener never answers within 10 ms.
        async fn request_timeout() -> reqwest::Error {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind a local port");
            let url = format!("http://{}/", listener.local_addr().expect("local address"));
            reqwest::Client::new()
                .get(url)
                .timeout(std::time::Duration::from_millis(10))
                .send()
                .await
                .expect_err("the listener never answers")
        }

        #[tokio::test]
        async fn connection_refused_is_unreachable() {
            let error = connection_refused().await;
            assert!(error.is_connect());
            let error = client_error(BlokliErrorKind::Reqwest(error));
            assert_eq!(error.failure_kind(), FailureKind::Unreachable);
        }

        #[tokio::test]
        async fn connection_refused_inside_cynic_is_unreachable() {
            let cynic = CynicReqwestError::ReqwestError(connection_refused().await);
            let error = EdgliError::BlokliClient(BlokliErrorKind::Cynic(cynic).into());
            assert_eq!(error.failure_kind(), FailureKind::Unreachable);
        }

        #[test]
        fn tls_failure_in_the_source_chain_is_unreachable() {
            let tls = rustls::Error::General("handshake failed".into());
            let error = EdgliError::Connector(ConnectorError::backend(std::io::Error::other(tls)));
            assert_eq!(error.failure_kind(), FailureKind::Unreachable);
        }

        #[test]
        fn unhealthy_blokli_is_service_unavailable() {
            let error = EdgliError::Connector(ConnectorError::ServerNotHealthy);
            assert_eq!(error.failure_kind(), FailureKind::ServiceUnavailable);
        }

        #[test]
        fn sync_timeout_is_service_unavailable() {
            let error = EdgliError::Connector(ConnectorError::ConnectionTimeout);
            assert_eq!(error.failure_kind(), FailureKind::ServiceUnavailable);
        }

        #[test]
        fn cached_unhealthy_blokli_is_service_unavailable() {
            let cached = ConnectorError::CacheError(Arc::new(ConnectorError::ServerNotHealthy));
            let error = EdgliError::Connector(cached);
            assert_eq!(error.failure_kind(), FailureKind::ServiceUnavailable);
        }

        #[test]
        fn client_timeout_is_service_unavailable() {
            let error = client_error(BlokliErrorKind::Timeout);
            assert_eq!(error.failure_kind(), FailureKind::ServiceUnavailable);
        }

        #[test]
        fn throttled_action_is_service_unavailable() {
            let error = client_error(BlokliErrorKind::HoprActionThrottled {
                operation: "announce".into(),
                reason: "repeated".into(),
                retry_after: std::time::Duration::from_secs(5),
                message: "slow down".into(),
            });
            assert_eq!(error.failure_kind(), FailureKind::ServiceUnavailable);
        }

        #[test]
        fn server_error_and_rate_limit_are_service_unavailable() {
            for status in [
                reqwest::StatusCode::BAD_GATEWAY,
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                reqwest::StatusCode::TOO_MANY_REQUESTS,
            ] {
                let cynic = CynicReqwestError::ErrorResponse(status, String::new());
                let error = client_error(BlokliErrorKind::Cynic(cynic));
                assert_eq!(
                    error.failure_kind(),
                    FailureKind::ServiceUnavailable,
                    "{status}"
                );
            }
        }

        #[tokio::test]
        async fn request_timeout_is_service_unavailable() {
            let error = request_timeout().await;
            assert!(error.is_timeout());
            let error = client_error(BlokliErrorKind::Reqwest(error));
            assert_eq!(error.failure_kind(), FailureKind::ServiceUnavailable);
        }

        #[test]
        fn unparseable_answers_are_incompatible() {
            let serde = serde_json::from_str::<u8>("not json").expect_err("invalid JSON");
            for kind in [
                BlokliErrorKind::ParseError,
                BlokliErrorKind::NoData,
                BlokliErrorKind::Serialization(serde),
                BlokliErrorKind::GraphQLError(cynic::GraphQlError::new(
                    "unknown field".into(),
                    None,
                    None,
                    None,
                )),
            ] {
                assert_eq!(client_error(kind).failure_kind(), FailureKind::Incompatible);
            }
        }

        #[test]
        fn chain_error_wrapped_by_hopr_lib_is_still_classified() {
            let error = EdgliError::from(HoprLibError::chain(ConnectorError::ServerNotHealthy));
            assert_eq!(error.failure_kind(), FailureKind::ServiceUnavailable);
        }

        #[test]
        fn client_bad_request_is_other() {
            let cynic =
                CynicReqwestError::ErrorResponse(reqwest::StatusCode::BAD_REQUEST, String::new());
            let error = client_error(BlokliErrorKind::Cynic(cynic));
            assert_eq!(error.failure_kind(), FailureKind::Other);
        }

        #[test]
        fn local_errors_are_other() {
            assert_eq!(
                EdgliError::ConfigError("x".into()).failure_kind(),
                FailureKind::Other
            );
            assert_eq!(
                EdgliError::DiscoveryStopped.failure_kind(),
                FailureKind::Other
            );
            let invalid_state =
                EdgliError::Connector(ConnectorError::InvalidState("not connected"));
            assert_eq!(invalid_state.failure_kind(), FailureKind::Other);
        }
    }
}
