use hopr_utils_chain_connector::reexports::alloy::{
    primitives::{Address, B256, Bytes, U256, address},
    providers::Provider,

};

use crate::{
    chain::{
        constants::{DEFAULT_TARGET_SUFFIX, DEPLOY_SAFE_MODULE_AND_INCLUDE_NODES_IDENTIFIER, WXHOPR_TOKEN_ADDRESS},
        errors::ChainError,
    },
};

// TicketStats object - not used in this snippet but may be relevant in the full file

#[derive(Clone, Debug)]
pub struct SafeModuleDeploymentInputs {
    pub token_amount: U256,
    pub nonce: U256,
    pub admins: Vec<Address>,
}

#[derive(Clone, Debug)]
pub struct SafeModuleDeploymentResult {
    pub tx_hash: B256,
    pub safe_address: Address,
    pub module_address: Address,
}


impl SafeModuleDeploymentInputs {
    pub fn new(nonce: U256, token_amount: U256, admins: Vec<Address>) -> Self {
        Self {
            nonce,
            token_amount,
            admins,
        }
    }

    /// Build user data equivalent to Solidity:
    /// `abi.encode(factory.DEPLOYSAFEMODULE_FUNCTION_IDENTIFIER(), nonce, DEFAULT_TARGET, admins)`
    /// Where:
    /// - DEPLOYSAFEMODULE_FUNCTION_IDENTIFIER = DEPLOY_SAFE_MODULE_AND_INCLUDE_NODES_IDENTIFIER
    /// - DEFAULT_TARGET = CHANNELS_CONTRACT_ADDRESS + DEFAULT_TARGET_SUFFIX as bytes32
    pub fn build_user_data(&self, network: &Network) -> Bytes {
        let default_target = NetworkSpecifications::get_network_contracts(network).build_default_target();

        let user_data_with_offset = UserDataTuple::abi_encode(&(
            DEPLOY_SAFE_MODULE_AND_INCLUDE_NODES_IDENTIFIER,
            self.nonce,
            default_target,
            self.admins.clone(),
        ));

        // remove the first 32 bytes which is the offset
        let user_data = user_data_with_offset[32..].to_vec();
        Bytes::from(user_data)
    }

    // blockli_client -> send transaction and wait for completion
    pub async fn deploy(
        &self,
        provider: &GnosisProvider,
        network: Network,
    ) -> Result<SafeModuleDeploymentResult, ChainError> {
        let token_instance = Token::new(WXHOPR_TOKEN_ADDRESS, provider.clone());
        // Implementation for deploying the safe module using the client
        let user_data = self.build_user_data(&network);

        // deploy the safe module by calling send on the wxHOPR token contract
        let pending_tx = token_instance
            .send(
                NetworkSpecifications::get_network_contracts(&network).node_stake_factory_address,
                self.token_amount,
                user_data,
            )
            .send()
            .await?;

        let receipt = pending_tx.get_receipt().await?;
        let maybe_safe_log = receipt.decoded_log::<HoprNodeStakeFactory::NewHoprNodeStakeSafe>();
        let Some(safe_log) = maybe_safe_log else {
            return Err(ChainError::DecodeEventError("NewHoprNodeStakeSafe".to_string()));
        };
        let maybe_module_log = receipt.decoded_log::<HoprNodeStakeFactory::NewHoprNodeStakeModule>();
        let Some(module_log) = maybe_module_log else {
            return Err(ChainError::DecodeEventError("NewHoprNodeStakeModule".to_string()));
        };

        Ok(SafeModuleDeploymentResult {
            tx_hash: receipt.transaction_hash,
            safe_address: safe_log.instance,
            module_address: module_log.instance,
        })
    }
}

// blokli_client -> query native balance, query token balance
#[derive(Clone, Debug)]
pub struct CheckBalanceInputs {
    pub hopr_token_holder: Address,
    pub native_token_holder: Address,
}

#[derive(Clone, Debug)]
pub struct CheckBalanceResult {
    pub hopr_token_balance: U256,
    pub native_token_balance: U256,
}

impl CheckBalanceInputs {
    pub fn new(hopr_token_holder: Address, native_token_holder: Address) -> Self {
        Self {
            hopr_token_holder,
            native_token_holder,
        }
    }

    pub async fn check(&self, provider: &GnosisProvider) -> Result<CheckBalanceResult, ChainError> {
        let token_instance = Token::new(WXHOPR_TOKEN_ADDRESS, provider.clone());

        let multicall = provider
            .multicall()
            .add(token_instance.balanceOf(self.hopr_token_holder))
            .get_eth_balance(self.native_token_holder);

        let (hopr_token_balance, native_token_balance) = multicall.aggregate().await?;

        Ok(CheckBalanceResult {
            hopr_token_balance,
            native_token_balance,
        })
    }
}

/// hopli -> build transaction
/// Send HOPR tokens to a recipient address
pub async fn send_hopr_tokens(provider: &GnosisProvider, recipient: Address, amount: U256) -> Result<B256, ChainError> {
    let token_instance = Token::new(WXHOPR_TOKEN_ADDRESS, provider.clone());
    let pending_tx = token_instance.send(recipient, amount, Bytes::new()).send().await?;
    let receipt = pending_tx.get_receipt().await?;

    Ok(receipt.transaction_hash)
}
