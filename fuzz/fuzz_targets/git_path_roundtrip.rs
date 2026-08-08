//! `GitPath` must have exactly one JSON identity per path.
//!
//! The raw-byte form exists so non-UTF-8 names survive a round trip, and it is
//! deliberately canonical: lowercase hex only, and never encoding something
//! that is valid UTF-8. Two encodings of one path would let a receipt describe
//! a change ambiguously.
#![no_main]

use change_capsule::GitPath;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(decoded) = serde_json::from_slice::<GitPath>(data) else {
        return;
    };
    // Anything that decodes must re-encode and decode again identically.
    let encoded = serde_json::to_vec(&decoded).expect("GitPath re-encodes");
    let again: GitPath = serde_json::from_slice(&encoded).expect("GitPath round trips");
    assert_eq!(decoded, again, "GitPath encoding is not canonical");
});
