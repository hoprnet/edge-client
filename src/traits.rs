//! Testable interface traits for the edge client.
//!
//! [`EdgeNodeApi`] encapsulates the operations that consumers such as
//! `gnosis_vpn-client` perform on an edge node.  By programming against this
//! trait rather than the concrete [`crate::Edgli`] type, consumers can
//! substitute a mock or stub implementation in unit tests without requiring a
//! live HOPR node or blockchain connection.

use hopr_lib::{
    api::{
        node::HoprState,
        types::primitive::prelude::{Address, HoprBalance},
    },
    errors::HoprLibError,
};

/// High-level edge node API for consumers.
///
/// All methods delegate to the underlying [`hopr_lib`] trait implementations
/// on the concrete [`crate::Edgli`] type.  Test code may implement this trait
/// with stubs or mocks to avoid requiring network connectivity.
#[async_trait::async_trait]
pub trait EdgeNodeApi: Send + Sync {
    /// The node's on-chain Ethereum address.
    fn me_onchain(&self) -> Address;

    /// The node's off-chain peer ID (libp2p string representation).
    fn me_peer_id(&self) -> String;

    /// Current node lifecycle state.
    fn status(&self) -> HoprState;

    /// HOPR token balance held directly by the node wallet.
    async fn get_balance(&self) -> std::result::Result<HoprBalance, HoprLibError>;

    /// HOPR token balance held inside the node's Safe.
    async fn get_safe_balance(&self) -> std::result::Result<HoprBalance, HoprLibError>;
}

#[cfg(feature = "runtime-tokio")]
mod impl_edgli {
    use super::*;
    use crate::client::Edgli;
    use hopr_lib::api::{
        node::{HoprNodeOperations, IncentiveChannelOperations},
        types::primitive::prelude::WxHOPR,
    };

    #[async_trait::async_trait]
    impl EdgeNodeApi for Edgli {
        fn me_onchain(&self) -> Address {
            Edgli::me_onchain(self)
        }

        fn me_peer_id(&self) -> String {
            Edgli::me_peer_id(self)
        }

        fn status(&self) -> HoprState {
            HoprNodeOperations::status(self.as_hopr().as_ref())
        }

        async fn get_balance(&self) -> std::result::Result<HoprBalance, HoprLibError> {
            let hopr = self.as_hopr();
            IncentiveChannelOperations::get_balance::<WxHOPR>(hopr.as_ref())
                .await
                .map_err(|e| HoprLibError::GeneralError(e.to_string()))
        }

        async fn get_safe_balance(&self) -> std::result::Result<HoprBalance, HoprLibError> {
            let hopr = self.as_hopr();
            IncentiveChannelOperations::get_safe_balance::<WxHOPR>(hopr.as_ref())
                .await
                .map_err(|e| HoprLibError::GeneralError(e.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hopr_lib::api::node::HoprState;
    use std::sync::Arc;

    /// A minimal stub that satisfies [`EdgeNodeApi`] for unit testing.
    struct StubEdgeNode {
        address: Address,
        peer_id: String,
        state: HoprState,
        balance: HoprBalance,
    }

    impl Default for StubEdgeNode {
        fn default() -> Self {
            Self {
                address: Address::default(),
                peer_id: "16Uiu2HAmStub000".into(),
                state: HoprState::Running,
                balance: HoprBalance::zero(),
            }
        }
    }

    #[async_trait::async_trait]
    impl EdgeNodeApi for StubEdgeNode {
        fn me_onchain(&self) -> Address {
            self.address
        }

        fn me_peer_id(&self) -> String {
            self.peer_id.clone()
        }

        fn status(&self) -> HoprState {
            self.state
        }

        async fn get_balance(&self) -> std::result::Result<HoprBalance, HoprLibError> {
            Ok(self.balance)
        }

        async fn get_safe_balance(&self) -> std::result::Result<HoprBalance, HoprLibError> {
            Ok(self.balance)
        }
    }

    #[test]
    fn stub_me_onchain_returns_configured_address() {
        let node = StubEdgeNode::default();
        assert_eq!(node.me_onchain(), Address::default());
    }

    #[test]
    fn stub_me_peer_id_returns_configured_peer_id() {
        let node = StubEdgeNode::default();
        assert_eq!(node.me_peer_id(), "16Uiu2HAmStub000");
    }

    #[test]
    fn stub_status_returns_running() {
        let node = StubEdgeNode::default();
        assert_eq!(node.status(), HoprState::Running);
    }

    #[tokio::test]
    async fn stub_get_balance_returns_zero() {
        let node = StubEdgeNode::default();
        assert_eq!(node.get_balance().await.unwrap(), HoprBalance::zero());
    }

    #[tokio::test]
    async fn stub_get_safe_balance_returns_zero() {
        let node = StubEdgeNode::default();
        assert_eq!(node.get_safe_balance().await.unwrap(), HoprBalance::zero());
    }

    #[test]
    fn arc_stub_direct_dispatch_works() {
        // Call trait methods on the concrete type via Arc (no dyn overhead needed).
        let node = Arc::new(StubEdgeNode::default());
        assert_eq!(node.status(), HoprState::Running);
        assert_eq!(node.me_onchain(), Address::default());
    }
}
