use std::num::NonZeroU16;

use davey::{DaveSession, ProposalsOperationType};

use crate::MediaError;

pub const DAVE_VERSION: u16 = 1;

pub struct DaveState {
    session: DaveSession,
    protocol_version: u16,
    user_id: u64,
    channel_id: u64,
    pending_transition: Option<u16>,
}

impl DaveState {
    pub fn new(user_id: u64, channel_id: u64) -> Result<Self, MediaError> {
        let version = NonZeroU16::new(DAVE_VERSION).expect("version");
        let session = DaveSession::new(version, user_id, channel_id, None)
            .map_err(|err| MediaError::Dave(err.to_string()))?;
        Ok(Self {
            session,
            protocol_version: 0,
            user_id,
            channel_id,
            pending_transition: None,
        })
    }

    pub fn set_protocol_version(&mut self, version: u16) {
        self.protocol_version = version;
    }

    pub fn protocol_version(&self) -> u16 {
        self.protocol_version
    }

    #[allow(dead_code)]
    pub fn ready(&self) -> bool {
        self.session.is_ready()
    }

    pub fn rekey(&mut self) -> Result<(), MediaError> {
        let version = NonZeroU16::new(self.protocol_version.max(DAVE_VERSION))
            .ok_or_else(|| MediaError::Dave("DAVE version is zero".into()))?;
        self.session
            .reinit(version, self.user_id, self.channel_id, None)
            .map_err(|err| MediaError::Dave(err.to_string()))
    }

    pub fn key_package(&mut self) -> Result<Vec<u8>, MediaError> {
        self.session
            .create_key_package()
            .map_err(|err| MediaError::Dave(err.to_string()))
    }

    pub fn external_sender(&mut self, bytes: &[u8]) -> Result<(), MediaError> {
        self.session
            .set_external_sender(bytes)
            .map_err(|err| MediaError::Dave(err.to_string()))
    }

    pub fn proposals(&mut self, operation: u8, proposals: &[u8]) -> Result<Option<davey::CommitWelcome>, MediaError> {
        let operation = match operation {
            0 => ProposalsOperationType::APPEND,
            1 => ProposalsOperationType::REVOKE,
            other => return Err(MediaError::Dave(format!("unknown proposal op {other}"))),
        };
        // Discord adds people to the MLS group before our roster sees them.
        // Rejecting those proposals aborts the key exchange, so audio and video stay undecrypted.
        self.session
            .process_proposals(operation, proposals, None)
            .map_err(|err| MediaError::Dave(err.to_string()))
    }

    pub fn commit(&mut self, commit: &[u8]) -> Result<(), MediaError> {
        self.session
            .process_commit(commit)
            .map_err(|err| MediaError::Dave(err.to_string()))
    }

    pub fn welcome(&mut self, welcome: &[u8]) -> Result<(), MediaError> {
        self.session
            .process_welcome(welcome)
            .map_err(|err| MediaError::Dave(err.to_string()))
    }

    pub fn note_transition(&mut self, transition_id: u16) {
        self.pending_transition = Some(transition_id);
    }

    #[allow(dead_code)]
    pub fn pending_transition(&self) -> Option<u16> {
        self.pending_transition
    }

    pub fn enable_passthrough(&mut self) {
        self.session.set_passthrough_mode(true, Some(10));
    }

    pub fn decrypt(&mut self, user_id: u64, video: bool, packet: &[u8]) -> Result<Vec<u8>, MediaError> {
        if self.protocol_version == 0 || self.session.can_passthrough(user_id) && !self.session.is_ready() {
            return Ok(packet.to_vec());
        }
        let media = if video {
            davey::MediaType::VIDEO
        } else {
            davey::MediaType::AUDIO
        };
        match self.session.decrypt(user_id, media, packet) {
            Ok(plain) => Ok(plain),
            Err(err) => {
                if self.session.can_passthrough(user_id) {
                    if let Some(stripped) = crate::decode::strip_dave_supplemental(packet) {
                        return Ok(stripped.to_vec());
                    }
                    return Ok(packet.to_vec());
                }
                Err(MediaError::Dave(err.to_string()))
            }
        }
    }
}

/// Server binary voice frames on gateway v8 are `sequence:u16be || opcode || payload`.
pub fn split_server_binary(bytes: &[u8]) -> Option<(u8, &[u8])> {
    if bytes.len() < 3 {
        return None;
    }
    Some((bytes[2], &bytes[3..]))
}

pub fn client_binary(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + payload.len());
    out.push(opcode);
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_version_eight_binary() {
        let bytes = [0x00, 0x04, 25, 9, 9];
        let (opcode, payload) = split_server_binary(&bytes).unwrap();
        assert_eq!(opcode, 25);
        assert_eq!(payload, &[9, 9]);
    }
}
