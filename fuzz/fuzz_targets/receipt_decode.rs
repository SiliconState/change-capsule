//! Decoding attacker-supplied receipt and policy JSON must never panic.
//!
//! Receipts travel between machines and `policy.json` is operator-supplied, so
//! both are untrusted input. The contract is that malformed input produces an
//! error, never an abort — and that anything which *does* decode can then be
//! projected into an attestation without panicking.
#![no_main]

use change_capsule::{ArtifactBundle, CapsuleResult, Policy, change_statement};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // A decoded result must survive projection into an in-toto statement.
    if let Ok(result) = serde_json::from_slice::<CapsuleResult>(data) {
        let statement = change_statement(&result);
        let _ = serde_json::to_vec(&statement);
    }
    let _ = serde_json::from_slice::<ArtifactBundle>(data);
    let _ = serde_json::from_slice::<Policy>(data);
});
