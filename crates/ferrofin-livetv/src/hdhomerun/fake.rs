//! A faithful fake HDHomeRun control device, for tests.
//!
//! The legacy tuning path is a WIRE PROTOCOL — a small binary get/set over TCP
//! 65001, then RTP to an address the server names — so it can be exercised
//! end to end without hardware by implementing the other side of that wire.
//! That is what this is: it parses the very packets
//! [`super::manager`] writes, answers them the way a device does, and streams
//! RTP at whatever `target` it is given.
//!
//! It deliberately shares NO code with the encoder under test beyond the two
//! public parsing helpers, so a framing bug cannot cancel itself out: the
//! request side is decoded here by hand, and the reply is encoded here by hand.
//!
//! This is a fake, not a device. It proves the protocol Ferrofin speaks is the
//! protocol it intends to speak, and that the receive/strip/buffer path works.
//! It cannot prove a real SiliconDust box agrees — see the module docs on
//! [`super`].

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};

/// The bytes the fake sends as one RTP packet's payload.
pub const PAYLOAD: &[u8] = b"FERROFINTSPAYLOAD";

/// One setting the device was told to set: `(name, value, lockkey)`.
type SeenCommand = (String, String, Option<u32>);

/// A fake device listening on an ephemeral port.
pub struct FakeDevice {
    /// Where the control protocol is served.
    pub addr: SocketAddr,
    /// Every `/tuner{n}/{name} = value` the device was asked to set.
    commands: Arc<Mutex<Vec<SeenCommand>>>,
}

impl FakeDevice {
    /// The settings the device has been asked to write, in order.
    #[must_use]
    pub fn commands(&self) -> Vec<SeenCommand> {
        self.commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Starts a device whose first `free_tuners` tuners report `lockkey=none`.
    ///
    /// `free_tuners == 0` models a box with every tuner in use, which is
    /// upstream's `LiveTvConflictException("No tuners available")` case.
    ///
    /// # Panics
    ///
    /// Panics when no loopback port can be bound — a test-harness failure, not
    /// a condition the caller can act on.
    pub async fn start(free_tuners: i32) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("listen");
        let addr = listener.local_addr().expect("addr");
        let commands = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&commands);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(serve(socket, free_tuners, Arc::clone(&seen)));
            }
        });
        Self { addr, commands }
    }
}

/// One control connection: read a request, answer it, repeat.
async fn serve(
    mut socket: tokio::net::TcpStream,
    free_tuners: i32,
    seen: Arc<Mutex<Vec<SeenCommand>>>,
) {
    let mut buffer = [0_u8; 8192];
    let mut rtp: Option<tokio::task::JoinHandle<()>> = None;
    while let Ok(received) = socket.read(&mut buffer).await {
        if received == 0 {
            break;
        }
        let Some(request) = parse_request(&buffer[..received]) else {
            break;
        };
        let value = match &request.value {
            None => {
                // A GET. The only one the server issues is `lockkey`, whose
                // answer decides whether the tuner is free.
                if request.tuner < free_tuners {
                    "none".to_owned()
                } else {
                    "locked".to_owned()
                }
            }
            Some(value) => {
                seen.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((request.name.clone(), value.clone(), request.lockkey));
                if request.name == "target" && value.starts_with("rtp://") {
                    if let Some(handle) = rtp.take() {
                        handle.abort();
                    }
                    if let Ok(target) = value.trim_start_matches("rtp://").parse::<SocketAddr>() {
                        rtp = Some(tokio::spawn(stream_rtp(target)));
                    }
                }
                value.clone()
            }
        };
        let reply = encode_reply(request.tuner, &request.name, &value);
        if socket.write_all(&reply).await.is_err() {
            break;
        }
    }
    if let Some(handle) = rtp.take() {
        handle.abort();
    }
}

/// Sends RTP-framed [`PAYLOAD`] packets at `target` until it is aborted.
async fn stream_rtp(target: SocketAddr) {
    let Ok(socket) = UdpSocket::bind("127.0.0.1:0").await else {
        return;
    };
    // 12 bytes of header — the fake's are arbitrary because the receiver only
    // ever skips them — then the payload.
    let mut packet = vec![0x80, 0x21, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    packet.extend_from_slice(PAYLOAD);
    loop {
        if socket.send_to(&packet, target).await.is_err() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// A decoded get/set request.
struct Request {
    /// The tuner index out of `/tuner{n}/{name}`.
    tuner: i32,
    /// The setting name out of `/tuner{n}/{name}`.
    name: String,
    /// The value, when this is a SET.
    value: Option<String>,
    /// The lock key, when one was attached.
    lockkey: Option<u32>,
}

/// Decodes one request packet, by hand: `[u16 type][u16 len][tag,len,str,0]…[u32 crc]`.
fn parse_request(packet: &[u8]) -> Option<Request> {
    if packet.len() < 8 || u16::from_be_bytes(packet[0..2].try_into().ok()?) != 4 {
        return None;
    }
    let declared = usize::from(u16::from_be_bytes(packet[2..4].try_into().ok()?));
    if packet.len() != 4 + declared + 4 {
        return None;
    }
    let body = &packet[4..4 + declared];
    let mut at = 0;
    let (mut path, mut value, mut lockkey) = (None, None, None);
    while at < body.len() {
        let tag = body[at];
        at += 1;
        match tag {
            // GetSetName / GetSetValue: a length-prefixed, null-terminated string.
            3 | 4 => {
                let len = usize::from(*body.get(at)?);
                at += 1;
                let text = String::from_utf8(body.get(at..at + len - 1)?.to_vec()).ok()?;
                at += len;
                if tag == 3 {
                    path = Some(text);
                } else {
                    value = Some(text);
                }
            }
            // GetSetLockkey: a one-byte length then a big-endian u32.
            21 => {
                at += 1;
                lockkey = Some(u32::from_be_bytes(body.get(at..at + 4)?.try_into().ok()?));
                at += 4;
            }
            _ => return None,
        }
    }
    let path = path?;
    let rest = path.strip_prefix("/tuner")?;
    let (tuner, name) = rest.split_once('/')?;
    Some(Request {
        tuner: tuner.parse().ok()?,
        name: name.to_owned(),
        value,
        lockkey,
    })
}

/// Encodes a `GetSetReply` for `/tuner{tuner}/{name} = value`, by hand.
fn encode_reply(tuner: i32, name: &str, value: &str) -> Vec<u8> {
    fn push_string(out: &mut Vec<u8>, tag: u8, text: &str) {
        out.push(tag);
        out.push(u8::try_from(text.len() + 1).expect("short"));
        out.extend_from_slice(text.as_bytes());
        out.push(0);
    }
    let mut body = Vec::new();
    push_string(&mut body, 3, &format!("/tuner{tuner}/{name}"));
    push_string(&mut body, 4, value);

    let mut packet = Vec::new();
    packet.extend_from_slice(&5_u16.to_be_bytes());
    packet.extend_from_slice(&u16::try_from(body.len()).expect("short").to_be_bytes());
    packet.extend_from_slice(&body);
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&packet);
    packet.extend_from_slice(&hasher.finalize().to_le_bytes());
    packet
}
