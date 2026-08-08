//! Optional detached Ed25519 authenticity for exported receipts.
//!
//! Signatures cover a domain-separated SHA-256 commitment of the exact
//! `bundle.json` bytes. The bundle descriptors in turn bind `result.json` and
//! `result.patch`. Public keys are supplied out of band by the verifier.

use std::io::Write;
use std::path::Path;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

use crate::error::{Error, Result, io};
use crate::state::read_bytes_bounded;

const DOMAIN: &[u8] = b"change-capsule receipt signature v1\0";
const BUNDLE_CAP: u64 = 1024 * 1024;
const SIGNATURE_BYTES: usize = 64;

/// A newly generated raw Ed25519 keypair.
///
/// The 32-byte private seed is zeroized when dropped. Callers should persist it
/// only in appropriately protected secret storage.
#[derive(Zeroize)]
#[zeroize(drop)]
#[non_exhaustive]
pub struct GeneratedKeypair {
    private_seed: [u8; 32],
    public_key: [u8; 32],
}

impl GeneratedKeypair {
    /// Borrow the raw 32-byte Ed25519 private seed.
    pub fn private_seed(&self) -> &[u8; 32] {
        &self.private_seed
    }

    /// Return the matching raw 32-byte Ed25519 public key.
    pub fn public_key(&self) -> [u8; 32] {
        self.public_key
    }
}

/// Generate a matching Ed25519 private seed and public key with the OS CSPRNG.
pub fn generate_keypair() -> Result<GeneratedKeypair> {
    let mut private_seed = [0_u8; 32];
    getrandom::fill(&mut private_seed)
        .map_err(|error| Error::InvalidInput(format!("OS random generation failed: {error}")))?;
    let public_key = derive_public_key(&private_seed);
    Ok(GeneratedKeypair {
        private_seed,
        public_key,
    })
}

/// Derive the raw 32-byte Ed25519 public key for a raw 32-byte private seed.
pub fn derive_public_key(private_seed: &[u8; 32]) -> [u8; 32] {
    let seed = Zeroizing::new(*private_seed);
    SigningKey::from_bytes(&seed).verifying_key().to_bytes()
}

/// Compute the fixed-domain commitment signed for receipt authenticity.
pub fn bundle_signature_commitment(bundle_json: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(DOMAIN);
    digest.update(bundle_json);
    digest.finalize().into()
}

/// Sign exact `bundle.json` bytes with an Ed25519 private seed.
///
/// The key is caller-owned and is never written to Capsule state.
pub fn sign_bundle_bytes(bundle_json: &[u8], private_seed: &[u8; 32]) -> [u8; 64] {
    let seed = Zeroizing::new(*private_seed);
    let key = SigningKey::from_bytes(&seed);
    key.sign(&bundle_signature_commitment(bundle_json))
        .to_bytes()
}

/// Verify a detached signature with an explicitly trusted Ed25519 public key.
pub fn verify_bundle_signature_bytes(
    bundle_json: &[u8],
    signature: &[u8; 64],
    trusted_public_key: &[u8; 32],
) -> Result<()> {
    let key = VerifyingKey::from_bytes(trusted_public_key)
        .map_err(|_| Error::Verification("trusted Ed25519 public key is invalid".to_owned()))?;
    let signature = Signature::from_bytes(signature);
    key.verify(&bundle_signature_commitment(bundle_json), &signature)
        .map_err(|_| Error::Verification("bundle signature is invalid".to_owned()))
}

/// Sign a receipt's exact `bundle.json` bytes and write a raw 64-byte signature.
pub fn sign_bundle(
    directory: impl AsRef<Path>,
    private_seed: &[u8; 32],
    signature_path: impl AsRef<Path>,
) -> Result<()> {
    let bundle = read_bytes_bounded(&directory.as_ref().join("bundle.json"), BUNDLE_CAP)?;
    let signature = sign_bundle_bytes(&bundle, private_seed);
    let path = signature_path.as_ref();
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut output = tempfile::NamedTempFile::new_in(parent).map_err(|error| io(parent, error))?;
    output
        .write_all(&signature)
        .and_then(|()| output.as_file().sync_all())
        .map_err(|error| io(output.path(), error))?;
    output.persist_noclobber(path).map_err(|error| {
        if error.error.kind() == std::io::ErrorKind::AlreadyExists {
            Error::InvalidInput(format!(
                "refusing to overwrite signature file: {}",
                path.display()
            ))
        } else {
            io(path, error.error)
        }
    })?;
    sync_parent_directory(parent)
}

/// Verify a receipt's raw detached signature with a trusted public key.
pub fn verify_bundle_signature(
    directory: impl AsRef<Path>,
    signature_path: impl AsRef<Path>,
    trusted_public_key: &[u8; 32],
) -> Result<()> {
    let bundle = read_bytes_bounded(&directory.as_ref().join("bundle.json"), BUNDLE_CAP)?;
    let path = signature_path.as_ref();
    let bytes = read_bytes_bounded(path, SIGNATURE_BYTES as u64)
        .map_err(|error| Error::Verification(format!("cannot read signature: {error}")))?;
    let signature: [u8; SIGNATURE_BYTES] = bytes.try_into().map_err(|_| {
        Error::Verification("signature file must contain exactly 64 raw bytes".to_owned())
    })?;
    verify_bundle_signature_bytes(&bundle, &signature, trusted_public_key)
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> Result<()> {
    std::fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| io(path, error))
}

/// Windows offers no portable directory-sync equivalent, so publication relies
/// on the file sync plus the atomic rename already performed by the caller.
#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn sync_parent_directory(_path: &Path) -> Result<()> {
    Ok(())
}
