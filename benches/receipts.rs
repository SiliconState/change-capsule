//! Benchmarks for the pure, Git-free parts of the receipt path.
//!
//! Everything here is CPU-bound and deterministic. Lifecycle operations are
//! deliberately excluded: they are dominated by Git subprocess time, which
//! measures the machine rather than this crate.

// `criterion_group!`/`criterion_main!` generate undocumented items.
#![allow(missing_docs)]

use std::collections::BTreeMap;
use std::hint::black_box;

use change_capsule::{
    CapsuleResult, GitPath, ResultKind, SCHEMA_VERSION, bundle_signature_commitment,
    change_statement, generate_keypair, sign_bundle_bytes, verify_bundle_signature_bytes,
};
use criterion::{Criterion, criterion_group, criterion_main};

/// A sealed result with a realistically large changed-path inventory.
fn wide_result(paths: usize) -> CapsuleResult {
    let mut result: CapsuleResult = serde_json::from_value(serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "capsule_id": "cap-01hbenchbenchbenchbenchbe",
        "kind": "patch",
        "base_commit": "0".repeat(40),
        "head_commit": "1".repeat(40),
        "patch_sha256": "2".repeat(64),
        "patch_bytes": 1_048_576_u64,
        "changed_paths": [],
        "ignored_content_sha256": "3".repeat(64),
        "created_at_unix": 1_700_000_000_u64,
        "sealed_at_unix": 1_700_000_100_u64,
    }))
    .expect("benchmark result fixture");
    result.kind = ResultKind::Patch;
    result.changed_paths = (0..paths)
        .map(|index| GitPath::Utf8(format!("crates/module{index}/src/lib.rs")))
        .collect();
    result.links = BTreeMap::from([("task".to_owned(), "bench".to_owned())]);
    result
}

fn attestation(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("attestation");
    for paths in [16_usize, 512, 4096] {
        let result = wide_result(paths);
        group.bench_function(format!("project_and_serialize/{paths}"), |bencher| {
            bencher.iter(|| {
                let statement = change_statement(black_box(&result));
                serde_json::to_vec(&statement).expect("serialize statement")
            });
        });
    }
    group.finish();
}

fn signatures(criterion: &mut Criterion) {
    let keys = generate_keypair().expect("keypair");
    let bundle = vec![b'{'; 4096];
    let signature = sign_bundle_bytes(&bundle, keys.private_seed());
    let public = keys.public_key();

    let mut group = criterion.benchmark_group("signature");
    group.bench_function("commitment", |bencher| {
        bencher.iter(|| bundle_signature_commitment(black_box(&bundle)));
    });
    group.bench_function("verify", |bencher| {
        bencher.iter(|| {
            verify_bundle_signature_bytes(black_box(&bundle), &signature, &public).is_ok()
        });
    });
    group.finish();
}

fn inventory_encoding(criterion: &mut Criterion) {
    // The raw-byte form is the costly one: it is the non-UTF-8 escape hatch.
    let raw = serde_json::to_vec(&GitPath::Utf8("src/very/deep/module/file.rs".to_owned()))
        .expect("encode");
    criterion.bench_function("git_path/decode", |bencher| {
        bencher.iter(|| serde_json::from_slice::<GitPath>(black_box(&raw)).expect("decode"));
    });
}

criterion_group!(benches, attestation, signatures, inventory_encoding);
criterion_main!(benches);
