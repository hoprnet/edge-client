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
        types::{
            internal::{channels::ChannelEntry, prelude::WinningProbability},
            primitive::prelude::{Address, Balance, HoprBalance, XDai},
        },
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
    // --- Identity ---

    /// The node's on-chain Ethereum address.
    fn me_onchain(&self) -> Address;

    /// The node's off-chain peer ID (libp2p string representation).
    fn me_peer_id(&self) -> String;

    /// The Safe contract address associated with this node.
    fn safe_address(&self) -> Address;

    // --- Node state ---

    /// Current node lifecycle state.
    fn status(&self) -> HoprState;

    // --- Balances ---

    /// WxHOPR token balance held directly by the node wallet.
    async fn get_balance(&self) -> std::result::Result<HoprBalance, HoprLibError>;

    /// WxHOPR token balance held inside the node's Safe.
    async fn get_safe_balance(&self) -> std::result::Result<HoprBalance, HoprLibError>;

    /// xDAI (native gas token) balance held directly by the node wallet.
    async fn get_xdai_balance(&self) -> std::result::Result<Balance<XDai>, HoprLibError>;

    // --- Channels ---

    /// All outgoing channels originating from this node (any status).
    ///
    /// Includes `Open`, `PendingToClose`, and `Closed` channels.
    async fn my_outgoing_channels(&self) -> std::result::Result<Vec<ChannelEntry>, HoprLibError>;

    /// Open a payment channel to `target` funded with `amount` WxHOPR.
    ///
    /// Submits an on-chain transaction and waits for confirmation.
    /// Returns `Ok(())` on success, or an error if the channel cannot be opened.
    async fn open_channel(
        &self,
        target: Address,
        amount: HoprBalance,
    ) -> std::result::Result<(), HoprLibError>;

    // --- Ticket / incentive ---

    /// Current minimum ticket price set by the network.
    async fn get_ticket_price(&self) -> std::result::Result<HoprBalance, HoprLibError>;

    /// Current minimum incoming ticket winning probability set by the network.
    async fn get_ticket_win_probability(
        &self,
    ) -> std::result::Result<WinningProbability, HoprLibError>;

    // --- Peer discovery ---

    /// On-chain addresses of all currently connected peers.
    ///
    /// Combines the transport-layer peer list with the chain-key lookup so
    /// callers receive Ethereum addresses directly.
    async fn connected_peer_addresses(&self) -> std::result::Result<Vec<Address>, HoprLibError>;
}

#[cfg(feature = "runtime-tokio")]
mod impl_edgli {
    use super::*;
    use crate::client::Edgli;
    use hopr_lib::api::{
        chain::ChainKeyOperations,
        node::{HasChainApi, HasTransportApi, HoprNodeOperations, IncentiveChannelOperations},
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

        fn safe_address(&self) -> Address {
            let hopr = self.as_hopr();
            HasChainApi::identity(hopr.as_ref()).safe_address
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

        async fn get_xdai_balance(&self) -> std::result::Result<Balance<XDai>, HoprLibError> {
            let hopr = self.as_hopr();
            IncentiveChannelOperations::get_balance::<XDai>(hopr.as_ref())
                .await
                .map_err(|e| HoprLibError::GeneralError(e.to_string()))
        }

        async fn my_outgoing_channels(
            &self,
        ) -> std::result::Result<Vec<ChannelEntry>, HoprLibError> {
            let hopr = self.as_hopr();
            let source = HasChainApi::identity(hopr.as_ref()).node_address;
            IncentiveChannelOperations::channels_from(hopr.as_ref(), source)
                .await
                .map_err(|e| HoprLibError::GeneralError(e.to_string()))
        }

        async fn open_channel(
            &self,
            target: Address,
            amount: HoprBalance,
        ) -> std::result::Result<(), HoprLibError> {
            let hopr = self.as_hopr();
            IncentiveChannelOperations::open_channel(hopr.as_ref(), target, amount)
                .await
                .map(|_| ())
                .map_err(|e| HoprLibError::GeneralError(e.to_string()))
        }

        async fn get_ticket_price(&self) -> std::result::Result<HoprBalance, HoprLibError> {
            let hopr = self.as_hopr();
            IncentiveChannelOperations::get_ticket_price(hopr.as_ref())
                .await
                .map_err(|e| HoprLibError::GeneralError(e.to_string()))
        }

        async fn get_ticket_win_probability(
            &self,
        ) -> std::result::Result<WinningProbability, HoprLibError> {
            let hopr = self.as_hopr();
            IncentiveChannelOperations::get_minimum_incoming_ticket_win_probability(hopr.as_ref())
                .await
                .map_err(|e| HoprLibError::GeneralError(e.to_string()))
        }

        async fn connected_peer_addresses(
            &self,
        ) -> std::result::Result<Vec<Address>, HoprLibError> {
            let hopr = self.as_hopr();
            let offchain_keys = HasTransportApi::transport(hopr.as_ref())
                .network_connected_peers()
                .await
                .map_err(|e| HoprLibError::GeneralError(e.to_string()))?;

            let mut addresses = Vec::new();
            for key in offchain_keys {
                match hopr.chain_api().packet_key_to_chain_key(&key) {
                    Ok(Some(address)) => addresses.push(address),
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(%key, error = ?e, "failed to get chain address for offchain key");
                    }
                }
            }
            Ok(addresses)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A minimal stub that satisfies [`EdgeNodeApi`] for unit testing.
    struct StubEdgeNode {
        address: Address,
        safe: Address,
        peer_id: String,
        state: HoprState,
        balance: HoprBalance,
        xdai: Balance<XDai>,
        channels: Vec<ChannelEntry>,
        ticket_price: HoprBalance,
        win_prob: WinningProbability,
        peers: Vec<Address>,
    }

    impl Default for StubEdgeNode {
        fn default() -> Self {
            Self {
                address: Address::default(),
                safe: Address::default(),
                peer_id: "16Uiu2HAmStub000".into(),
                state: HoprState::Running,
                balance: HoprBalance::zero(),
                xdai: Balance::zero(),
                channels: vec![],
                ticket_price: HoprBalance::zero(),
                win_prob: WinningProbability::ALWAYS,
                peers: vec![],
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

        fn safe_address(&self) -> Address {
            self.safe
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

        async fn get_xdai_balance(&self) -> std::result::Result<Balance<XDai>, HoprLibError> {
            Ok(self.xdai)
        }

        async fn my_outgoing_channels(
            &self,
        ) -> std::result::Result<Vec<ChannelEntry>, HoprLibError> {
            Ok(self.channels.clone())
        }

        async fn open_channel(
            &self,
            _target: Address,
            _amount: HoprBalance,
        ) -> std::result::Result<(), HoprLibError> {
            Ok(())
        }

        async fn get_ticket_price(&self) -> std::result::Result<HoprBalance, HoprLibError> {
            Ok(self.ticket_price)
        }

        async fn get_ticket_win_probability(
            &self,
        ) -> std::result::Result<WinningProbability, HoprLibError> {
            Ok(self.win_prob)
        }

        async fn connected_peer_addresses(
            &self,
        ) -> std::result::Result<Vec<Address>, HoprLibError> {
            Ok(self.peers.clone())
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
    fn stub_safe_address_returns_configured_address() {
        let node = StubEdgeNode::default();
        assert_eq!(node.safe_address(), Address::default());
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

    #[tokio::test]
    async fn stub_get_xdai_balance_returns_zero() {
        let node = StubEdgeNode::default();
        assert_eq!(node.get_xdai_balance().await.unwrap(), Balance::zero());
    }

    #[tokio::test]
    async fn stub_my_outgoing_channels_returns_empty() {
        let node = StubEdgeNode::default();
        assert!(node.my_outgoing_channels().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn stub_open_channel_succeeds() {
        let node = StubEdgeNode::default();
        assert!(
            node.open_channel(Address::default(), HoprBalance::zero())
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn stub_get_ticket_price_returns_zero() {
        let node = StubEdgeNode::default();
        assert_eq!(node.get_ticket_price().await.unwrap(), HoprBalance::zero());
    }

    #[tokio::test]
    async fn stub_get_ticket_win_probability_returns_always() {
        let node = StubEdgeNode::default();
        let prob = node.get_ticket_win_probability().await.unwrap();
        // WinningProbability only implements PartialEq<f64> and PartialEq<EncodedWinProb>,
        // not PartialEq<WinningProbability>. Compare via f64 conversion instead.
        assert_eq!(prob, 1.0_f64);
    }

    #[tokio::test]
    async fn stub_connected_peer_addresses_returns_empty() {
        let node = StubEdgeNode::default();
        assert!(node.connected_peer_addresses().await.unwrap().is_empty());
    }

    #[test]
    fn arc_stub_direct_dispatch_works() {
        // Call trait methods on the concrete type via Arc (no dyn overhead needed).
        let node = Arc::new(StubEdgeNode::default());
        assert_eq!(node.status(), HoprState::Running);
        assert_eq!(node.me_onchain(), Address::default());
    }
}
