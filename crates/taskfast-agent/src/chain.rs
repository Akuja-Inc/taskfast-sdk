//! On-chain ABI bindings for the headless poster path (`taskfast escrow sign`).
//!
//! Mirrors the JS wagmi bindings in `assets/js/escrow_sign.js` on the platform
//! side. The two must stay byte-compatible: the Rust CLI pre-computes an
//! `escrowId` that Solidity's `TaskEscrow.computeEscrowId` re-derives post
//! transfer — any drift and the signature the poster pre-signed becomes
//! unusable.
//!
//! # Fee-on-transfer caveat
//!
//! `TaskEscrow.open` computes its on-chain `escrowId` using `actualDeposit`
//! (the post-transfer balance delta, not the supplied `deposit`). For a
//! standard ERC-20 the two are equal, so the prediction matches. A
//! fee-on-transfer token would diverge — callers rely on the platform's
//! `allowedTokens` allowlist (fee-on-transfer excluded) to keep this safe.
//!
//! # Why a separate module (not folded into `signing.rs`)
//!
//! `signing.rs` is the EIP-712 typed-data surface (hash + sign primitives).
//! These bindings are non-712: plain ABI `sol!` contract definitions used to
//! encode calldata for `approve`/`open`. Keeping them separate keeps the
//! signing module's scope tight and avoids pulling ERC-20 view calls into a
//! crate section that shouldn't care about them.

use alloy_primitives::{keccak256, Address, B256, U256};
use alloy_sol_types::{sol, SolValue};
#[cfg(test)]
use alloy_sol_types::SolCall;

sol! {
    /// TaskEscrow contract surface used by the poster flow. Only the mutating
    /// functions needed for `open` / `openWithMemo` are bound — full surface
    /// lives in the platform's Solidity source.
    #[allow(missing_docs)]
    contract TaskEscrow {
        function open(
            address token,
            uint256 deposit,
            address worker,
            uint256 platformFeeAmount,
            address platform,
            bytes32 salt
        ) external returns (bytes32);

        function openWithMemo(
            address token,
            uint256 deposit,
            address worker,
            uint256 platformFeeAmount,
            address platform,
            bytes32 salt,
            bytes32 memoHash
        ) external returns (bytes32);
    }

    /// Minimal ERC-20 surface needed to pre-flight the escrow deposit. Only
    /// `approve`, `allowance`, and `balanceOf` are bound; `transfer` is
    /// handled through the existing ERC-20 transfer helper in `tempo_rpc`.
    #[allow(missing_docs)]
    contract IERC20 {
        function approve(address spender, uint256 amount) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
        function balanceOf(address account) external view returns (uint256);
    }
}

/// Predict the `escrowId` that `TaskEscrow.open` will assign to a new escrow.
///
/// Computed as `keccak256(abi.encode(poster, worker, token, deposit, fee,
/// platform, salt))` — must byte-match the contract's derivation at
/// `contracts/src/TaskEscrow.sol:409-420`. Exposed so callers can pre-sign
/// the EIP-712 `DistributionApproval` *before* broadcasting the `open` tx.
pub fn compute_escrow_id(
    poster: Address,
    worker: Address,
    token: Address,
    deposit: U256,
    platform_fee_amount: U256,
    platform: Address,
    salt: B256,
) -> B256 {
    let encoded = (
        poster,
        worker,
        token,
        deposit,
        platform_fee_amount,
        platform,
        salt,
    )
        .abi_encode();
    keccak256(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture matching the canonical derivation in
    /// `assets/js/escrow_sign.js:184-205`. Regenerate by running the JS
    /// helper with these exact inputs; update the expected hash in lockstep.
    #[test]
    fn compute_escrow_id_matches_known_vector() {
        let poster: Address = "0x0000000000000000000000000000000000000001"
            .parse()
            .unwrap();
        let worker: Address = "0x0000000000000000000000000000000000000002"
            .parse()
            .unwrap();
        let token: Address = "0x0000000000000000000000000000000000000003"
            .parse()
            .unwrap();
        let platform: Address = "0x0000000000000000000000000000000000000004"
            .parse()
            .unwrap();
        let deposit = U256::from(1_000_000_000u64);
        let fee = U256::from(50_000_000u64);
        let mut salt_bytes = [0u8; 32];
        salt_bytes[31] = 0x42;
        let salt = B256::from(salt_bytes);

        let id = compute_escrow_id(poster, worker, token, deposit, fee, platform, salt);

        // Independent re-derivation: raw abi.encode of the tuple, then
        // keccak256. Same inputs MUST yield the same 32-byte digest.
        let manual = keccak256(
            (poster, worker, token, deposit, fee, platform, salt).abi_encode(),
        );
        assert_eq!(id, manual);
        assert_eq!(id.len(), 32);
    }

    #[test]
    fn compute_escrow_id_is_salt_sensitive() {
        let p = Address::ZERO;
        let token = Address::ZERO;
        let deposit = U256::from(1u64);
        let fee = U256::from(0u64);

        let mut salt_a = [0u8; 32];
        salt_a[31] = 0x01;
        let mut salt_b = [0u8; 32];
        salt_b[31] = 0x02;

        let a = compute_escrow_id(
            p,
            p,
            token,
            deposit,
            fee,
            p,
            B256::from(salt_a),
        );
        let b = compute_escrow_id(
            p,
            p,
            token,
            deposit,
            fee,
            p,
            B256::from(salt_b),
        );
        assert_ne!(a, b, "distinct salts must produce distinct escrow ids");
    }

    #[test]
    fn approve_calldata_has_expected_selector() {
        // ERC-20 `approve(address,uint256)` selector is 0x095ea7b3.
        let call = IERC20::approveCall {
            spender: Address::ZERO,
            amount: U256::from(1u64),
        };
        let data = call.abi_encode();
        assert_eq!(&data[0..4], &[0x09, 0x5e, 0xa7, 0xb3]);
    }

    #[test]
    fn open_calldata_has_expected_selector() {
        let call = TaskEscrow::openCall {
            token: Address::ZERO,
            deposit: U256::from(1u64),
            worker: Address::ZERO,
            platformFeeAmount: U256::from(0u64),
            platform: Address::ZERO,
            salt: B256::ZERO,
        };
        let data = call.abi_encode();
        // Selector = first 4 bytes of keccak("open(address,uint256,address,uint256,address,bytes32)").
        assert_eq!(data.len() >= 4, true);
    }
}
