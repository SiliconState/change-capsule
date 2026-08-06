//! Streaming and publication of sealed artifacts.
//!
//! Artifacts are exposed as bounded in-memory snapshots that were validated
//! before being handed out, so a later filesystem mutation cannot change bytes
//! already paired with a descriptor. Transport is the caller's concern: this
//! crate assigns no cloud, object-store, or content-addressed backend.

use std::io::{Cursor, Read};

use serde::{Deserialize, Serialize};

use crate::{ArtifactDescriptor, Result};

/// A bounded reader over one validated artifact snapshot.
#[derive(Debug)]
pub struct ArtifactReader {
    inner: Cursor<Vec<u8>>,
}

impl ArtifactReader {
    pub(crate) fn new(bytes: Vec<u8>) -> Self {
        Self {
            inner: Cursor::new(bytes),
        }
    }
}

impl Read for ArtifactReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buffer)
    }
}

/// Runtime-neutral destination for sealed artifacts.
///
/// Implementations may write to a local directory, object storage, or a
/// content-addressed store. The returned string is the durable URI assigned by
/// the sink. Artifact bytes are provided as a bounded stream.
pub trait ArtifactSink {
    /// Store one artifact and return the durable URI assigned to it.
    ///
    /// Implementations are caller code and inherit the caller's authentication,
    /// retention, and transport responsibilities.
    fn put(&mut self, descriptor: &ArtifactDescriptor, source: &mut dyn Read) -> Result<String>;
}

/// An artifact that a sink accepted, paired with the URI it was given.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedArtifact {
    /// Descriptor of the bytes that were published.
    pub descriptor: ArtifactDescriptor,
    /// Durable URI the sink assigned.
    pub uri: String,
}
