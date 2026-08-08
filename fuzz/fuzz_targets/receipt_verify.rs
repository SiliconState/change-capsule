//! Verifying a hostile receipt directory must fail, never panic or escape.
//!
//! This drives the real entry point a reviewer or merge gate calls on a receipt
//! that arrived from somewhere else. The split below lets the fuzzer vary all
//! three artifacts independently, including sizes and digests that disagree.
#![no_main]

use std::fs;

use change_capsule::{VerifyOptions, attest_bundle, verify_bundle};
use libfuzzer_sys::fuzz_target;

/// Split input into three artifacts using two length prefixes.
fn split(data: &[u8]) -> Option<(&[u8], &[u8], &[u8])> {
    let (first, rest) = data.split_at_checked(2)?;
    let bundle_len = usize::from(u16::from_le_bytes([first[0], first[1]]));
    let (second, rest) = rest.split_at_checked(2)?;
    let result_len = usize::from(u16::from_le_bytes([second[0], second[1]]));
    let (bundle, rest) = rest.split_at_checked(bundle_len.min(rest.len()))?;
    let (result, patch) = rest.split_at_checked(result_len.min(rest.len()))?;
    Some((bundle, result, patch))
}

fuzz_target!(|data: &[u8]| {
    let Some((bundle, result, patch)) = split(data) else {
        return;
    };
    let Ok(directory) = tempfile::tempdir() else {
        return;
    };
    let root = directory.path();
    if fs::write(root.join("bundle.json"), bundle).is_err()
        || fs::write(root.join("result.json"), result).is_err()
        || fs::write(root.join("result.patch"), patch).is_err()
    {
        return;
    }

    // No repository: this target is about parsing and digest checking, not Git.
    let options = VerifyOptions::new(false, false, None);
    let verified = verify_bundle(root, &options).is_ok();

    // Attestation must succeed exactly when verification does. A statement for
    // a receipt that did not verify would launder it.
    let attested = attest_bundle(root, &options).is_ok();
    assert_eq!(
        verified, attested,
        "attestation and verification must agree on every receipt"
    );
});
