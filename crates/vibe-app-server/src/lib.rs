#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod server;

use vibe_protocol::{Envelope, ProtocolValidationError, decode_frame, encode_frame};

pub trait SerializedBoundary {
    fn receive(&mut self, frame: &[u8]) -> Result<(), ProtocolValidationError>;
    fn send(&mut self) -> Result<Option<Vec<u8>>, ProtocolValidationError>;
}

#[derive(Default)]
pub struct MemoryBoundary {
    pending: Option<Envelope>,
}

impl SerializedBoundary for MemoryBoundary {
    fn receive(&mut self, frame: &[u8]) -> Result<(), ProtocolValidationError> {
        self.pending = Some(decode_frame(frame)?);
        Ok(())
    }

    fn send(&mut self) -> Result<Option<Vec<u8>>, ProtocolValidationError> {
        self.pending
            .take()
            .map(|frame| encode_frame(&frame))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_boundary_serializes_instead_of_sharing_objects() {
        let mut boundary = MemoryBoundary::default();
        let frame = br#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#;
        boundary.receive(frame).expect("valid fixture");
        assert_eq!(
            boundary.send().expect("serializable fixture"),
            Some(frame.to_vec())
        );
    }
}
