use std::io::{Cursor, Read};

use serde::{Deserialize, Serialize};

use crate::{ArtifactDescriptor, Result};

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
    fn put(&mut self, descriptor: &ArtifactDescriptor, source: &mut dyn Read) -> Result<String>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedArtifact {
    pub descriptor: ArtifactDescriptor,
    pub uri: String,
}
