use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use thiserror::Error;

pub const WORKER_ARG: &str = "--internal-av1-worker-v1";
pub const FLAG_ABORT: u32 = 1;
pub const FLAG_HANG: u32 = 2;
pub const FLAG_BAD_ID: u32 = 4;

const MAGIC: &[u8; 4] = b"OMT1";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 34;
const MAX_INPUT: u32 = 8 * 1024 * 1024;
pub const MAX_WIDTH: u32 = 3840;
pub const MAX_HEIGHT: u32 = 2160;
pub const MAX_OUTPUT: u32 = MAX_WIDTH * MAX_HEIGHT * 4;

pub const KIND_HELLO: u8 = 1;
pub const KIND_READY: u8 = 2;
pub const KIND_DECODE: u8 = 3;
pub const KIND_RESET: u8 = 4;
pub const KIND_DROP: u8 = 5;
pub const KIND_PICTURE: u8 = 6;
pub const KIND_NO_PICTURE: u8 = 7;
pub const KIND_DECODE_ERROR: u8 = 8;

#[derive(Debug, Error)]
pub enum Av1IpcError {
    #[error("av1 worker io: {0}")]
    Io(String),
    #[error("av1 worker protocol: {0}")]
    Protocol(String),
    #[error("av1 worker timed out")]
    Timeout,
    #[error("av1 decode failed: {0}")]
    Decode(String),
    #[error("av1 worker is paused after repeated crashes")]
    Paused,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub kind: u8,
    pub flags: u32,
    pub request_id: u64,
    pub ssrc: u32,
    pub width: u32,
    pub height: u32,
    pub payload: Vec<u8>,
}

pub fn write_message(writer: &mut impl Write, message: &Message) -> Result<(), Av1IpcError> {
    if message.payload.len() > MAX_OUTPUT as usize {
        return Err(Av1IpcError::Protocol("payload exceeds the frame limit".into()));
    }
    let mut header = [0u8; HEADER_LEN];
    header[0..4].copy_from_slice(MAGIC);
    header[4] = VERSION;
    header[5] = message.kind;
    header[6..10].copy_from_slice(&message.flags.to_le_bytes());
    header[10..18].copy_from_slice(&message.request_id.to_le_bytes());
    header[18..22].copy_from_slice(&message.ssrc.to_le_bytes());
    header[22..26].copy_from_slice(&message.width.to_le_bytes());
    header[26..30].copy_from_slice(&message.height.to_le_bytes());
    header[30..34].copy_from_slice(&(message.payload.len() as u32).to_le_bytes());
    writer.write_all(&header).map_err(|err| Av1IpcError::Io(err.to_string()))?;
    writer.write_all(&message.payload).map_err(|err| Av1IpcError::Io(err.to_string()))?;
    writer.flush().map_err(|err| Av1IpcError::Io(err.to_string()))?;
    Ok(())
}

pub fn read_message(reader: &mut impl Read) -> Result<Message, Av1IpcError> {
    let mut header = [0u8; HEADER_LEN];
    reader.read_exact(&mut header).map_err(|err| Av1IpcError::Io(err.to_string()))?;
    if &header[0..4] != MAGIC || header[4] != VERSION {
        return Err(Av1IpcError::Protocol("unexpected header".into()));
    }
    let kind = header[5];
    if !matches!(
        kind,
        KIND_HELLO | KIND_READY | KIND_DECODE | KIND_RESET | KIND_DROP | KIND_PICTURE | KIND_NO_PICTURE | KIND_DECODE_ERROR
    ) {
        return Err(Av1IpcError::Protocol(format!("unknown message kind {kind}")));
    }
    let payload_len = u32::from_le_bytes(header[30..34].try_into().unwrap());
    let limit = if kind == KIND_DECODE || kind == KIND_HELLO { MAX_INPUT } else { MAX_OUTPUT };
    if payload_len > limit {
        return Err(Av1IpcError::Protocol(format!("payload length {payload_len} is too large")));
    }
    let mut payload = vec![0u8; payload_len as usize];
    if payload_len > 0 {
        reader.read_exact(&mut payload).map_err(|err| Av1IpcError::Io(err.to_string()))?;
    }
    Ok(Message {
        kind,
        flags: u32::from_le_bytes(header[6..10].try_into().unwrap()),
        request_id: u64::from_le_bytes(header[10..18].try_into().unwrap()),
        ssrc: u32::from_le_bytes(header[18..22].try_into().unwrap()),
        width: u32::from_le_bytes(header[22..26].try_into().unwrap()),
        height: u32::from_le_bytes(header[26..30].try_into().unwrap()),
        payload,
    })
}

pub struct Av1Supervisor {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    responses: Option<Receiver<Result<Message, Av1IpcError>>>,
    next_id: u64,
    timeout: Duration,
    crashes: Vec<Instant>,
    restarts: u32,
    paused_until: Option<Instant>,
}

impl Av1Supervisor {
    pub fn spawn() -> Result<Self, Av1IpcError> {
        Self::spawn_with(Duration::from_secs(5))
    }

    pub fn spawn_with(timeout: Duration) -> Result<Self, Av1IpcError> {
        let mut supervisor = Self {
            child: None,
            stdin: None,
            responses: None,
            next_id: 1,
            timeout,
            crashes: Vec::new(),
            restarts: 0,
            paused_until: None,
        };
        supervisor.start()?;
        Ok(supervisor)
    }

    pub fn decode(&mut self, ssrc: u32, obu: &[u8]) -> Result<Option<(Vec<u8>, u32, u32)>, Av1IpcError> {
        self.decode_flags(ssrc, obu, 0)
    }

    pub fn decode_flags(&mut self, ssrc: u32, obu: &[u8], flags: u32) -> Result<Option<(Vec<u8>, u32, u32)>, Av1IpcError> {
        self.roundtrip(KIND_DECODE, ssrc, flags, 0, 0, obu.to_vec())
    }

    pub fn reset_stream(&mut self, ssrc: u32) -> Result<(), Av1IpcError> {
        self.roundtrip(KIND_RESET, ssrc, 0, 0, 0, Vec::new()).map(|_| ())
    }

    fn roundtrip(
        &mut self,
        kind: u8,
        ssrc: u32,
        flags: u32,
        width: u32,
        height: u32,
        payload: Vec<u8>,
    ) -> Result<Option<(Vec<u8>, u32, u32)>, Av1IpcError> {
        self.ensure()?;
        let request_id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let message = Message { kind, flags, request_id, ssrc, width, height, payload };
        if let Err(err) = self.write(&message) {
            return Err(self.fail(err));
        }
        match self.read_matching(request_id) {
            Ok(message) => self.accept(message),
            Err(err) => Err(self.fail(err)),
        }
    }

    fn accept(&mut self, message: Message) -> Result<Option<(Vec<u8>, u32, u32)>, Av1IpcError> {
        match message.kind {
            KIND_PICTURE => {
                if message.width == 0 || message.height == 0 || message.width > MAX_WIDTH || message.height > MAX_HEIGHT {
                    return Err(self.fail(Av1IpcError::Protocol("picture dimensions are invalid".into())));
                }
                let expected = message.width as usize * message.height as usize * 4;
                if message.payload.len() != expected {
                    return Err(self.fail(Av1IpcError::Protocol("picture size does not match its dimensions".into())));
                }
                self.restarts = 0;
                Ok(Some((message.payload, message.width, message.height)))
            }
            KIND_NO_PICTURE | KIND_READY => {
                self.restarts = 0;
                Ok(None)
            }
            KIND_DECODE_ERROR => Err(Av1IpcError::Decode(String::from_utf8_lossy(&message.payload).into_owned())),
            other => Err(self.fail(Av1IpcError::Protocol(format!("unexpected reply {other}")))),
        }
    }

    fn ensure(&mut self) -> Result<(), Av1IpcError> {
        if let Some(until) = self.paused_until {
            if Instant::now() < until {
                return Err(Av1IpcError::Paused);
            }
            self.paused_until = None;
            self.crashes.clear();
            self.restarts = 0;
        }
        if self.alive() {
            return Ok(());
        }
        if self.restarts > 0 {
            std::thread::sleep(backoff(self.restarts));
        }
        self.start()
    }

    fn start(&mut self) -> Result<(), Av1IpcError> {
        self.stop();
        let exe = std::env::current_exe().map_err(|err| Av1IpcError::Io(err.to_string()))?;
        let mut command = Command::new(exe);
        command.arg(WORKER_ARG).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let mut child = command.spawn().map_err(|err| Av1IpcError::Io(err.to_string()))?;
        let stdin = child.stdin.take().ok_or_else(|| Av1IpcError::Io("worker stdin missing".into()))?;
        let mut stdout = child.stdout.take().ok_or_else(|| Av1IpcError::Io("worker stdout missing".into()))?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            loop {
                match read_message(&mut stdout) {
                    Ok(message) => {
                        if tx.send(Ok(message)).is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(Err(err));
                        break;
                    }
                }
            }
        });
        self.child = Some(child);
        self.stdin = Some(stdin);
        self.responses = Some(rx);
        let hello = Message {
            kind: KIND_HELLO,
            flags: 0,
            request_id: 0,
            ssrc: 0,
            width: 0,
            height: 0,
            payload: Vec::new(),
        };
        if let Err(err) = self.write(&hello) {
            return Err(self.fail(err));
        }
        match self.read_matching(0) {
            Ok(message) if message.kind == KIND_READY => Ok(()),
            Ok(_) => Err(self.fail(Av1IpcError::Protocol("worker did not become ready".into()))),
            Err(err) => Err(self.fail(err)),
        }
    }

    fn write(&mut self, message: &Message) -> Result<(), Av1IpcError> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(Av1IpcError::Io("worker is not running".into()));
        };
        write_message(stdin, message)
    }

    fn read_matching(&mut self, request_id: u64) -> Result<Message, Av1IpcError> {
        let Some(responses) = self.responses.as_ref() else {
            return Err(Av1IpcError::Io("worker is not running".into()));
        };
        match responses.recv_timeout(self.timeout) {
            Ok(Ok(message)) if message.request_id == request_id => Ok(message),
            Ok(Ok(_)) => Err(Av1IpcError::Protocol("response id did not match the request".into())),
            Ok(Err(err)) => Err(err),
            Err(RecvTimeoutError::Timeout) => Err(Av1IpcError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(Av1IpcError::Io("worker closed the response pipe".into())),
        }
    }

    fn alive(&mut self) -> bool {
        let Some(child) = self.child.as_mut() else {
            return false;
        };
        matches!(child.try_wait(), Ok(None))
    }

    fn fail(&mut self, err: Av1IpcError) -> Av1IpcError {
        self.stop();
        let now = Instant::now();
        self.crashes.retain(|crash| now.duration_since(*crash) < Duration::from_secs(30));
        self.crashes.push(now);
        self.restarts = self.restarts.saturating_add(1);
        if self.crashes.len() >= 5 {
            self.paused_until = Some(now + Duration::from_secs(30));
            tracing::warn!("av1 worker paused after repeated crashes");
        } else {
            tracing::warn!(error = %err, "av1 worker failed and will restart");
        }
        err
    }

    fn stop(&mut self) {
        self.stdin.take();
        self.responses.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Av1Supervisor {
    fn drop(&mut self) {
        self.stop();
    }
}

fn backoff(restarts: u32) -> Duration {
    match restarts {
        0 | 1 => Duration::from_millis(100),
        2 => Duration::from_millis(500),
        3 => Duration::from_secs(2),
        _ => Duration::from_secs(5),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn header(kind: u8, request_id: u64, payload_len: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; HEADER_LEN];
        bytes[0..4].copy_from_slice(MAGIC);
        bytes[4] = VERSION;
        bytes[5] = kind;
        bytes[10..18].copy_from_slice(&request_id.to_le_bytes());
        bytes[30..34].copy_from_slice(&payload_len.to_le_bytes());
        bytes
    }

    #[test]
    fn a_short_message_is_rejected() {
        let mut input = Cursor::new([0u8; 8]);
        assert!(read_message(&mut input).is_err());
    }

    #[test]
    fn an_oversize_message_is_rejected() {
        let mut input = Cursor::new(header(KIND_DECODE, 1, MAX_INPUT + 1));
        assert!(matches!(read_message(&mut input), Err(Av1IpcError::Protocol(_))));
    }

    #[test]
    fn an_unknown_kind_is_rejected() {
        let mut input = Cursor::new(header(99, 1, 0));
        assert!(matches!(read_message(&mut input), Err(Av1IpcError::Protocol(_))));
    }

    #[test]
    fn a_matching_roundtrip_preserves_the_request_id() {
        let message = Message {
            kind: KIND_NO_PICTURE,
            flags: 0,
            request_id: 7,
            ssrc: 11,
            width: 0,
            height: 0,
            payload: Vec::new(),
        };
        let mut bytes = Vec::new();
        write_message(&mut bytes, &message).unwrap();
        let decoded = read_message(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(decoded.request_id, 7);
        assert_eq!(decoded.ssrc, 11);
        assert_eq!(decoded.kind, KIND_NO_PICTURE);
    }
}
