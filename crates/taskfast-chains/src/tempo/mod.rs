//! Tempo chain — EIP-712 typed-data + raw payload signing.
//!
//! Moved verbatim from `taskfast-agent/src/signing.rs` (beads am-6v7b.4) behind
//! the `tempo` cargo feature. Byte-identical behaviour is load-bearing: the
//! cross-check fixture `cross_check_digest_matches_elixir_fixture` mirrors the
//! Elixir platform digest — any drift forks signature recoverability.
//!
//! Replaces `cast wallet sign --no-hash` with `alloy-sol-types` directly so the
//! binary has no Foundry dependency. Two signing surfaces:
//!
//! - [`sign_distribution`] — the production path for `taskfast settle`.
//!   Hashes a [`DistributionApproval`] struct against the TaskEscrow EIP-712
//!   domain and signs the resulting 32-byte digest with the caller's key.
//! - [`sign_hash_raw`]     — escape hatch for ad-hoc message hashes that the
//!   server asks the agent to sign (non-712 flows).
//!
//! # Why local domain constructors (not the `eip712_domain!` macro)
//!
//! The macro produces a `const Eip712Domain`, which forces `verifying_contract`
//! into a const slot. The contract address is runtime config (returned by the
//! readiness endpoint), so we build the [`Eip712Domain`] at call time with
//! `Eip712Domain::new`. `name` and `version` are still compile-time constants.
//!
//! # Cross-reference
//!
//! Platform-side signer: `lib/task_fast/payments/tempo_wallet_signer.ex`.
//! Chain IDs pinned in `lib/task_fast/payments/tempo_constants.ex` —
//! mainnet=4217, testnet=42431.

use std::borrow::Cow;

use alloy_primitives::{Address, Signature, B256, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{sol, Eip712Domain, SolStruct};
use thiserror::Error as ThisError;

use crate::chain::Chain;

pub const TEMPO_MAINNET_CHAIN_ID: u64 = 4_217;
pub const TEMPO_TESTNET_CHAIN_ID: u64 = 42_431;

/// Anvil (Foundry) default local chain id.
pub const ANVIL_LOCAL_CHAIN_ID: u64 = 31_337;
/// Hardhat default local chain id.
pub const HARDHAT_LOCAL_CHAIN_ID: u64 = 1_337;

/// PathUSD is the only token the platform will ever ask the CLI to move as
/// a submission fee. It happens to share the same address on mainnet + moderato,
/// but we key on `chain_id` anyway so a future divergence is a one-line
/// change here and nowhere else.
///
/// Source: <https://docs.tempo.xyz/quickstart/connection-details>.
pub const PATHUSD_MAINNET: &str = "0x20c0000000000000000000000000000000000000";
pub const PATHUSD_TESTNET: &str = "0x20c0000000000000000000000000000000000000";

/// F1 allowlist for the submission-fee ERC-20. Compares case-insensitively.
///
/// A compromised TaskFast server (or MITM on a stolen PAT) could return an
/// attacker-controlled `token_address` in the `create_task_draft` response
/// and steer the CLI to sign + broadcast an ERC-20 `transfer` against an
/// arbitrary contract. The allowlist turns that silent fund-drain into a
/// loud validation error.
///
/// Local-dev chains (anvil 31337, hardhat 1337) bypass the check — those
/// networks carry no real value and developers deploy mock USDC at arbitrary
/// addresses. The caller is responsible for refusing to run on mainnet without
/// the guard; [`is_known_network`] tells you which chains the allowlist covers.
pub fn is_allowed_fee_token(chain_id: u64, token_address_hex: &str) -> bool {
    let normalized = normalize_addr(token_address_hex);
    match chain_id {
        TEMPO_MAINNET_CHAIN_ID => normalized == normalize_addr(PATHUSD_MAINNET),
        TEMPO_TESTNET_CHAIN_ID => normalized == normalize_addr(PATHUSD_TESTNET),
        ANVIL_LOCAL_CHAIN_ID | HARDHAT_LOCAL_CHAIN_ID => true,
        _ => false,
    }
}

/// True when `chain_id` is covered by the allowlist (either a production
/// Tempo network or a recognized local-dev chain).
pub fn is_known_network(chain_id: u64) -> bool {
    matches!(
        chain_id,
        TEMPO_MAINNET_CHAIN_ID
            | TEMPO_TESTNET_CHAIN_ID
            | ANVIL_LOCAL_CHAIN_ID
            | HARDHAT_LOCAL_CHAIN_ID
    )
}

fn normalize_addr(s: &str) -> String {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    stripped.to_ascii_lowercase()
}

pub const TASK_ESCROW_DOMAIN_NAME: &str = "TaskEscrow";
pub const TASK_ESCROW_DOMAIN_VERSION: &str = "1";

sol! {
    struct DistributionApproval {
        bytes32 escrowId;
        uint256 deadline;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Testnet,
    Mainnet,
}

impl Network {
    pub fn chain_id(self) -> u64 {
        match self {
            Self::Testnet => TEMPO_TESTNET_CHAIN_ID,
            Self::Mainnet => TEMPO_MAINNET_CHAIN_ID,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Tempo {
    pub network: Network,
}

impl Tempo {
    pub fn new(network: Network) -> Self {
        Self { network }
    }
    pub fn testnet() -> Self {
        Self::new(Network::Testnet)
    }
    pub fn mainnet() -> Self {
        Self::new(Network::Mainnet)
    }
}

impl Chain for Tempo {
    type Address = Address;
    type Signature = String;
    type TxHash = String;
    type EscrowRef = B256;
    type Network = Network;

    fn id() -> &'static str {
        "tempo"
    }
    fn network(&self) -> &Self::Network {
        &self.network
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DistributionDomain {
    pub chain_id: u64,
    pub verifying_contract: Address,
}

impl DistributionDomain {
    pub fn new(chain_id: u64, verifying_contract: Address) -> Self {
        Self {
            chain_id,
            verifying_contract,
        }
    }

    pub fn testnet(verifying_contract: Address) -> Self {
        Self::new(TEMPO_TESTNET_CHAIN_ID, verifying_contract)
    }

    pub fn mainnet(verifying_contract: Address) -> Self {
        Self::new(TEMPO_MAINNET_CHAIN_ID, verifying_contract)
    }

    fn as_eip712(&self) -> Eip712Domain {
        Eip712Domain::new(
            Some(Cow::Borrowed(TASK_ESCROW_DOMAIN_NAME)),
            Some(Cow::Borrowed(TASK_ESCROW_DOMAIN_VERSION)),
            Some(U256::from(self.chain_id)),
            Some(self.verifying_contract),
            None,
        )
    }
}

#[derive(Debug, ThisError)]
pub enum SigningError {
    #[error("signer failed to produce signature: {0}")]
    SignFailed(String),
    #[error("signature hex is not valid: {0}")]
    InvalidSignatureHex(String),
    #[error("failed to recover signer address: {0}")]
    RecoveryFailed(String),
}

pub fn distribution_digest(domain: &DistributionDomain, escrow_id: B256, deadline: U256) -> B256 {
    let approval = DistributionApproval {
        escrowId: escrow_id,
        deadline,
    };
    approval.eip712_signing_hash(&domain.as_eip712())
}

pub fn sign_distribution(
    signer: &PrivateKeySigner,
    domain: &DistributionDomain,
    escrow_id: B256,
    deadline: U256,
) -> Result<String, SigningError> {
    let digest = distribution_digest(domain, escrow_id, deadline);
    sign_hash_raw(signer, digest)
}

pub fn sign_hash_raw(signer: &PrivateKeySigner, digest: B256) -> Result<String, SigningError> {
    let sig = signer
        .sign_hash_sync(&digest)
        .map_err(|e| SigningError::SignFailed(e.to_string()))?;
    Ok(encode_signature(&sig))
}

pub fn verify_distribution(
    signature_hex: &str,
    domain: &DistributionDomain,
    escrow_id: B256,
    deadline: U256,
    expected: Address,
) -> Result<bool, SigningError> {
    let sig = parse_signature(signature_hex)?;
    let digest = distribution_digest(domain, escrow_id, deadline);
    let recovered = sig
        .recover_address_from_prehash(&digest)
        .map_err(|e| SigningError::RecoveryFailed(e.to_string()))?;
    Ok(recovered == expected)
}

fn encode_signature(sig: &Signature) -> String {
    let mut out = String::with_capacity(2 + 65 * 2);
    out.push_str("0x");
    out.push_str(&hex::encode(sig.as_bytes()));
    out
}

pub(crate) fn parse_signature(hex_str: &str) -> Result<Signature, SigningError> {
    let stripped = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    let bytes =
        hex::decode(stripped).map_err(|e| SigningError::InvalidSignatureHex(e.to_string()))?;
    Signature::try_from(bytes.as_slice())
        .map_err(|e| SigningError::InvalidSignatureHex(e.to_string()))
}

#[cfg(test)]
mod allowlist_tests {
    use super::*;

    #[test]
    fn pathusd_is_allowed_on_mainnet() {
        assert!(is_allowed_fee_token(
            TEMPO_MAINNET_CHAIN_ID,
            PATHUSD_MAINNET
        ));
    }

    #[test]
    fn pathusd_is_allowed_on_testnet() {
        assert!(is_allowed_fee_token(
            TEMPO_TESTNET_CHAIN_ID,
            PATHUSD_TESTNET
        ));
    }

    #[test]
    fn mixed_case_hex_matches() {
        // `to_checksum` output has mixed case; the allowlist must not care.
        let checksummed = "0x20C0000000000000000000000000000000000000";
        assert!(is_allowed_fee_token(TEMPO_MAINNET_CHAIN_ID, checksummed));
    }

    #[test]
    fn attacker_token_rejected_on_mainnet() {
        let evil = "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        assert!(!is_allowed_fee_token(TEMPO_MAINNET_CHAIN_ID, evil));
    }

    #[test]
    fn attacker_token_rejected_on_testnet() {
        let evil = "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        assert!(!is_allowed_fee_token(TEMPO_TESTNET_CHAIN_ID, evil));
    }

    #[test]
    fn local_chains_bypass_allowlist() {
        // Anvil + hardhat must accept any token — devs deploy mock USDC
        // at whatever address `anvil_setCode` hands them.
        let any = "0x1234567890123456789012345678901234567890";
        assert!(is_allowed_fee_token(ANVIL_LOCAL_CHAIN_ID, any));
        assert!(is_allowed_fee_token(HARDHAT_LOCAL_CHAIN_ID, any));
    }

    #[test]
    fn unknown_chains_reject_everything() {
        // Default deny: an unrecognized chain_id is not in the allowlist,
        // so every token is rejected. Shields against a malicious RPC
        // returning `eth_chainId=1` (mainnet Ethereum) to pretend the
        // transfer is benign.
        assert!(!is_allowed_fee_token(1, PATHUSD_MAINNET));
        assert!(!is_allowed_fee_token(0, PATHUSD_MAINNET));
    }

    #[test]
    fn is_known_network_covers_both_tempo_and_local() {
        assert!(is_known_network(TEMPO_MAINNET_CHAIN_ID));
        assert!(is_known_network(TEMPO_TESTNET_CHAIN_ID));
        assert!(is_known_network(ANVIL_LOCAL_CHAIN_ID));
        assert!(is_known_network(HARDHAT_LOCAL_CHAIN_ID));
        assert!(!is_known_network(1));
    }
}
