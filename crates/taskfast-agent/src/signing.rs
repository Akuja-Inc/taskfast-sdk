//! EIP-712 typed-data + raw payload signing.
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

/// Tempo chain IDs, mirrored from `TempoConstants` on the platform. Keeping
/// them here instead of plumbing through config means the CLI can't
/// accidentally sign for the wrong network just because an env var is wrong.
pub const TEMPO_MAINNET_CHAIN_ID: u64 = 4_217;
pub const TEMPO_TESTNET_CHAIN_ID: u64 = 42_431;

/// EIP-712 domain identity for the TaskEscrow contract. Matches the
/// platform's `@domain_name` / `@domain_version`.
pub const TASK_ESCROW_DOMAIN_NAME: &str = "TaskEscrow";
pub const TASK_ESCROW_DOMAIN_VERSION: &str = "1";

sol! {
    /// The typed-data struct settled tasks are signed against. Field order +
    /// names are load-bearing for the EIP-712 typehash — must match the
    /// Solidity contract's struct exactly. Changing either field here forks
    /// the domain and every historical signature becomes unrecoverable.
    struct DistributionApproval {
        bytes32 escrowId;
        uint256 deadline;
    }
}

/// Runtime-resolved domain for the TaskEscrow contract.
///
/// The domain name + version are pinned at compile time (they've never
/// changed platform-side). `chain_id` and `verifying_contract` come from the
/// readiness endpoint so the same binary works on testnet and mainnet.
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

    /// Inflate into the alloy-flavored [`Eip712Domain`] used by `SolStruct`.
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

/// Errors surfaced by the signing APIs. Kept local (not folded into
/// [`taskfast_client::Error`]) because these are pure crypto-layer failures
/// with no HTTP component — a consumer catching [`taskfast_client::Error`]
/// shouldn't pattern-match "signature recovery failed" as a network issue.
#[derive(Debug, ThisError)]
pub enum SigningError {
    #[error("signer failed to produce signature: {0}")]
    SignFailed(String),
    #[error("signature hex is not valid: {0}")]
    InvalidSignatureHex(String),
    #[error("failed to recover signer address: {0}")]
    RecoveryFailed(String),
}

/// Compute the 32-byte EIP-712 digest for a [`DistributionApproval`] against
/// the given domain. Exposed so callers (and tests) can inspect or
/// cross-check the hash the contract will recover against.
pub fn distribution_digest(domain: &DistributionDomain, escrow_id: B256, deadline: U256) -> B256 {
    let approval = DistributionApproval {
        escrowId: escrow_id,
        deadline,
    };
    approval.eip712_signing_hash(&domain.as_eip712())
}

/// Sign a [`DistributionApproval`] and return `0x`-prefixed `r||s||v` hex
/// (132 chars total) — the shape the platform's `/tasks/:id/settle` endpoint
/// expects in its `signature` field.
pub fn sign_distribution(
    signer: &PrivateKeySigner,
    domain: &DistributionDomain,
    escrow_id: B256,
    deadline: U256,
) -> Result<String, SigningError> {
    let digest = distribution_digest(domain, escrow_id, deadline);
    sign_hash_raw(signer, digest)
}

/// Sign an arbitrary 32-byte digest without any EIP-191 prefix. Used for
/// EIP-712 flows (via [`sign_distribution`]) and for ad-hoc hashes the
/// server asks the agent to sign in its webhook payload.
pub fn sign_hash_raw(signer: &PrivateKeySigner, digest: B256) -> Result<String, SigningError> {
    let sig = signer
        .sign_hash_sync(&digest)
        .map_err(|e| SigningError::SignFailed(e.to_string()))?;
    Ok(encode_signature(&sig))
}

/// Recover the signer's address from a signature over a
/// [`DistributionApproval`] and assert it matches `expected`.
///
/// Returns `Ok(true)` iff the recovered address equals `expected`. Decode
/// errors (malformed hex) surface as `Err` rather than `Ok(false)` so the
/// caller can distinguish "invalid input" from "valid input, wrong signer".
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

fn parse_signature(hex_str: &str) -> Result<Signature, SigningError> {
    let stripped = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    let bytes =
        hex::decode(stripped).map_err(|e| SigningError::InvalidSignatureHex(e.to_string()))?;
    Signature::try_from(bytes.as_slice())
        .map_err(|e| SigningError::InvalidSignatureHex(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_domain() -> DistributionDomain {
        // Deterministic verifying contract for test assertions.
        let vc: Address = "0x00000000000000000000000000000000000000ee"
            .parse()
            .unwrap();
        DistributionDomain::testnet(vc)
    }

    fn fixed_escrow_id() -> B256 {
        // keccak("test-escrow-id") lookalike — any 32 bytes will do.
        let mut b = [0u8; 32];
        b[31] = 0x42;
        B256::from(b)
    }

    #[test]
    fn chain_id_constructors_are_correct() {
        let vc = Address::ZERO;
        assert_eq!(DistributionDomain::mainnet(vc).chain_id, 4_217);
        assert_eq!(DistributionDomain::testnet(vc).chain_id, 42_431);
    }

    #[test]
    fn digest_is_deterministic_for_same_inputs() {
        let d1 = distribution_digest(&fixed_domain(), fixed_escrow_id(), U256::from(100u64));
        let d2 = distribution_digest(&fixed_domain(), fixed_escrow_id(), U256::from(100u64));
        assert_eq!(d1, d2);
    }

    #[test]
    fn digest_differs_when_chain_id_differs() {
        // Same struct values on testnet vs mainnet MUST produce different
        // digests — otherwise a testnet settlement sig would be replayable
        // on mainnet.
        let vc: Address = "0x00000000000000000000000000000000000000ee"
            .parse()
            .unwrap();
        let testnet = DistributionDomain::testnet(vc);
        let mainnet = DistributionDomain::mainnet(vc);
        let a = distribution_digest(&testnet, fixed_escrow_id(), U256::from(100u64));
        let b = distribution_digest(&mainnet, fixed_escrow_id(), U256::from(100u64));
        assert_ne!(a, b, "chain_id must be bound into the domain separator");
    }

    #[test]
    fn digest_differs_when_escrow_id_differs() {
        let domain = fixed_domain();
        let a = distribution_digest(&domain, fixed_escrow_id(), U256::from(100u64));
        let mut other = [0u8; 32];
        other[31] = 0x43;
        let b = distribution_digest(&domain, B256::from(other), U256::from(100u64));
        assert_ne!(a, b);
    }

    #[test]
    fn signature_hex_has_expected_shape() {
        let signer = PrivateKeySigner::random();
        let sig = sign_distribution(
            &signer,
            &fixed_domain(),
            fixed_escrow_id(),
            U256::from(100u64),
        )
        .expect("sign");
        // 0x + 65 bytes * 2 hex chars/byte = 132 chars.
        assert_eq!(sig.len(), 132);
        assert!(sig.starts_with("0x"));
        assert!(sig[2..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sign_then_recover_roundtrip() {
        let signer = PrivateKeySigner::random();
        let expected = signer.address();
        let sig = sign_distribution(
            &signer,
            &fixed_domain(),
            fixed_escrow_id(),
            U256::from(1_700_000_000u64),
        )
        .expect("sign");
        let ok = verify_distribution(
            &sig,
            &fixed_domain(),
            fixed_escrow_id(),
            U256::from(1_700_000_000u64),
            expected,
        )
        .expect("verify");
        assert!(ok);
    }

    #[test]
    fn tampered_deadline_fails_verification() {
        let signer = PrivateKeySigner::random();
        let expected = signer.address();
        let sig = sign_distribution(
            &signer,
            &fixed_domain(),
            fixed_escrow_id(),
            U256::from(1_700_000_000u64),
        )
        .expect("sign");
        // Off-by-one on deadline → recovery still succeeds but yields a
        // different address, so verify_distribution returns Ok(false).
        let ok = verify_distribution(
            &sig,
            &fixed_domain(),
            fixed_escrow_id(),
            U256::from(1_700_000_001u64),
            expected,
        )
        .expect("verify");
        assert!(!ok, "tampered deadline must not verify against signer");
    }

    #[test]
    fn tampered_escrow_id_fails_verification() {
        let signer = PrivateKeySigner::random();
        let expected = signer.address();
        let sig = sign_distribution(
            &signer,
            &fixed_domain(),
            fixed_escrow_id(),
            U256::from(100u64),
        )
        .expect("sign");
        let mut other = [0u8; 32];
        other[31] = 0x99;
        let ok = verify_distribution(
            &sig,
            &fixed_domain(),
            B256::from(other),
            U256::from(100u64),
            expected,
        )
        .expect("verify");
        assert!(!ok);
    }

    #[test]
    fn cross_chain_replay_fails_verification() {
        // Sign on testnet, attempt to verify against mainnet domain with
        // the same struct values — must reject.
        let signer = PrivateKeySigner::random();
        let expected = signer.address();
        let vc: Address = "0x00000000000000000000000000000000000000ee"
            .parse()
            .unwrap();
        let testnet = DistributionDomain::testnet(vc);
        let mainnet = DistributionDomain::mainnet(vc);
        let sig = sign_distribution(&signer, &testnet, fixed_escrow_id(), U256::from(1u64))
            .expect("sign");
        let ok = verify_distribution(
            &sig,
            &mainnet,
            fixed_escrow_id(),
            U256::from(1u64),
            expected,
        )
        .expect("verify");
        assert!(!ok, "testnet signature must not verify on mainnet");
    }

    #[test]
    fn sign_hash_raw_roundtrips_via_prehash_recovery() {
        let signer = PrivateKeySigner::random();
        let expected = signer.address();
        let mut digest_bytes = [0u8; 32];
        digest_bytes[0] = 0xde;
        digest_bytes[1] = 0xad;
        digest_bytes[2] = 0xbe;
        digest_bytes[3] = 0xef;
        let digest = B256::from(digest_bytes);

        let sig_hex = sign_hash_raw(&signer, digest).expect("sign");
        let sig = parse_signature(&sig_hex).expect("parse");
        let recovered = sig.recover_address_from_prehash(&digest).expect("recover");
        assert_eq!(recovered, expected);
    }

    #[test]
    fn verify_rejects_malformed_signature_hex() {
        let err = verify_distribution(
            "0xnothex",
            &fixed_domain(),
            fixed_escrow_id(),
            U256::from(1u64),
            Address::ZERO,
        )
        .unwrap_err();
        assert!(matches!(err, SigningError::InvalidSignatureHex(_)));
    }

    #[test]
    fn verify_rejects_wrong_length_signature() {
        // 32 bytes instead of 65.
        let short = format!("0x{}", "ab".repeat(32));
        let err = verify_distribution(
            &short,
            &fixed_domain(),
            fixed_escrow_id(),
            U256::from(1u64),
            Address::ZERO,
        )
        .unwrap_err();
        assert!(matches!(err, SigningError::InvalidSignatureHex(_)));
    }

    /// Cross-check fixture: the 32-byte EIP-712 digest for this fixed vector
    /// MUST byte-equal the digest produced by the Elixir implementation in
    /// `lib/task_fast/payments/distribution_approval_verifier.ex`. If either
    /// implementation drifts, signatures produced on one side will not recover
    /// on the other. The mirror test lives in
    /// `test/task_fast/payments/distribution_approval_verifier_test.exs`.
    #[test]
    fn cross_check_digest_matches_elixir_fixture() {
        // escrow_id = repeat 0xab..0xab (32 bytes), deadline = 1_800_000_000,
        // verifying_contract = 0x00..01. Chain id is pinned testnet.
        let vc: Address = "0x0000000000000000000000000000000000000001"
            .parse()
            .unwrap();
        let domain = DistributionDomain::testnet(vc);
        let mut escrow_bytes = [0u8; 32];
        escrow_bytes.iter_mut().for_each(|b| *b = 0xab);
        let escrow_id = B256::from(escrow_bytes);
        let deadline = U256::from(1_800_000_000u64);

        let digest = distribution_digest(&domain, escrow_id, deadline);
        let hex = format!("0x{}", hex::encode(digest.as_slice()));

        // Regenerate with: MIX_ENV=test mix run -e \
        //   'IO.puts elem(TaskFast.Payments.DistributionApprovalVerifier.digest( \
        //     "0x" <> String.duplicate("ab", 32), 1_800_000_000, 42_431, \
        //     "0x0000000000000000000000000000000000000001"), 1) |> Base.encode16(case: :lower)'
        assert_eq!(
            hex.len(),
            66,
            "digest must be 32 bytes (66 chars incl 0x prefix)"
        );
        // Snapshot value — update in lockstep with the Elixir mirror test.
        assert_eq!(
            hex, "0xff4958335cd476ae06389497e736d3630ecee1b9b33cc65cbfd9c316dd2e3efb",
            "digest drifted — Elixir side must update in lockstep"
        );
    }

    #[test]
    fn parse_signature_tolerates_missing_0x_prefix() {
        let signer = PrivateKeySigner::random();
        let sig_with = sign_distribution(
            &signer,
            &fixed_domain(),
            fixed_escrow_id(),
            U256::from(1u64),
        )
        .expect("sign");
        let sig_without = sig_with.trim_start_matches("0x");
        let parsed_with = parse_signature(&sig_with).expect("with");
        let parsed_without = parse_signature(sig_without).expect("without");
        assert_eq!(parsed_with.as_bytes(), parsed_without.as_bytes());
    }
}
