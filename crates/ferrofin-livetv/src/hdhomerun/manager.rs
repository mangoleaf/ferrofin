//! The HDHomeRun TCP control protocol.
//!
//! Port of `HdHomerunManager.cs` and the two `IHdHomerunChannelCommands`
//! implementations (v10.11.8 `src/Jellyfin.LiveTv/TunerHosts/HdHomerun/`).
//!
//! A legacy HDHomeRun is tuned by talking a small binary get/set protocol to
//! TCP port 65001: claim a free tuner with a random lock key, set the channel
//! (and, on a transcoding model, the profile), then point the tuner's `target`
//! at an RTP URL this server is listening on. Every packet is
//! `[u16 type][u16 payload len][payload][u32 CRC]` with the two lengths
//! big-endian and the CRC little-endian.
//!
//! The framing lives in free functions on purpose: it is exactly what upstream
//! unit-tests (`HdHomerunManagerTests.cs` is 11 pure byte/CRC facts and no
//! socket), so the same facts transliterate directly.

use std::net::{IpAddr, SocketAddr};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use ferrofin_traits::error::ServiceError;

use crate::error::LiveTvError;

/// `HdHomerunManager.HdHomeRunPort` (v10.11.8 HdHomerunManager.cs:19).
pub const HD_HOMERUN_PORT: u16 = 65001;

/// `HdHomerunManager.GetSetName` — the tag introducing a setting's name.
const GET_SET_NAME: u8 = 3;
/// `HdHomerunManager.GetSetValue` — the tag introducing a setting's value.
const GET_SET_VALUE: u8 = 4;
/// `HdHomerunManager.GetSetLockkey` — the tag introducing the lock key.
const GET_SET_LOCKKEY: u8 = 21;
/// `HdHomerunManager.GetSetRequest` — the request packet type.
const GET_SET_REQUEST: u16 = 4;
/// `HdHomerunManager.GetSetReply` — the reply packet type.
const GET_SET_REPLY: u16 = 5;

/// `MediaBrowser.Common.Crc32.Compute` — the standard reflected zlib CRC-32,
/// whose own upstream tests pin `0x414f_a339` for
/// `"The quick brown fox jumps over the lazy dog"`.
fn crc32(bytes: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(bytes);
    hasher.finalize()
}

/// One `(name, value)` the device is told to set when a channel is tuned.
///
/// Port of `IHdHomerunChannelCommands.GetCommands`.
pub type ChannelCommand = (String, String);

/// `HdHomerunChannelCommands` (v10.11.8 HdHomerunChannelCommands.cs) — the
/// modern `vchannel` form, carrying the transcode profile inline when one is
/// selected and it is not the native passthrough.
#[must_use]
pub fn channel_commands(channel: Option<&str>, profile: Option<&str>) -> Vec<ChannelCommand> {
    let Some(channel) = channel.filter(|c| !c.is_empty()) else {
        return Vec::new();
    };
    let value = match profile {
        Some(profile) if !profile.is_empty() && !profile.eq_ignore_ascii_case("native") => {
            format!("{channel} transcode={profile}")
        }
        _ => channel.to_owned(),
    };
    vec![("vchannel".to_owned(), value)]
}

/// `LegacyHdHomerunChannelCommands` (v10.11.8 LegacyHdHomerunChannelCommands.cs)
/// — the pre-`vchannel` form, whose channel and program are parsed out of the
/// device URL with `@"\/ch([0-9]+)-?([0-9]*)"`.
#[must_use]
pub fn legacy_channel_commands(url: &str) -> Vec<ChannelCommand> {
    let mut commands = Vec::new();
    let Some((channel, program)) = parse_legacy_channel_url(url) else {
        return commands;
    };
    if !channel.is_empty() {
        commands.push(("channel".to_owned(), channel));
    }
    if !program.is_empty() {
        commands.push(("program".to_owned(), program));
    }
    commands
}

/// The `@"\/ch([0-9]+)-?([0-9]*)"` match, hand-rolled so the port carries no
/// regex dependency for one two-group pattern: find `"/ch"`, take the run of
/// digits after it, then an optional `-` and a second run of digits. `Match`
/// is unanchored and takes the FIRST occurrence, as `Regex.Match` does.
fn parse_legacy_channel_url(url: &str) -> Option<(String, String)> {
    let bytes = url.as_bytes();
    let mut from = 0;
    while let Some(offset) = url[from..].find("/ch") {
        let start = from + offset + 3;
        let mut i = start;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            // No digits: `([0-9]+)` needs at least one, so this `/ch` is not a
            // match and the scan continues past it.
            from = start;
            continue;
        }
        let channel = url[start..i].to_owned();
        // `-?` then `([0-9]*)`, which happily matches empty.
        let mut j = i;
        if j < bytes.len() && bytes[j] == b'-' {
            j += 1;
        }
        let prog_start = j;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        return Some((channel, url[prog_start..j].to_owned()));
    }
    None
}

/// `HdHomerunManager.WriteNullTerminatedString` (v10.11.8 HdHomerunManager.cs:242-253):
/// a one-byte length (which counts the terminator), the UTF-8 bytes, then a
/// `0`. Returns the number of bytes written.
///
/// The upstream `// TODO: variable length` comment is real: a payload of 127
/// bytes or more would need a two-byte length, and `Convert.ToByte` throws
/// instead. Ported as a hard error rather than a silent truncation.
///
/// # Errors
///
/// Fails when the payload does not fit a one-byte length, or the buffer is too
/// small — both of which upstream surfaces as an exception.
pub fn write_null_terminated_string(buffer: &mut [u8], payload: &str) -> Result<usize, String> {
    let bytes = payload.as_bytes();
    let len = bytes.len() + 1;
    if u8::try_from(len).is_err() {
        return Err(format!(
            "hdhomerun payload of {len} bytes exceeds one length byte"
        ));
    }
    if buffer.len() < len + 1 {
        return Err("hdhomerun packet buffer too small".to_owned());
    }
    buffer[0] = u8::try_from(len).unwrap_or(u8::MAX);
    buffer[1..len].copy_from_slice(bytes);
    buffer[len] = 0;
    Ok(len + 1)
}

/// `HdHomerunManager.WriteHeaderAndPayload` (v10.11.8 HdHomerunManager.cs:255-270):
/// the request type, a two-byte hole for the payload length, the name tag and
/// the name itself.
fn write_header_and_payload(buffer: &mut [u8], payload: &str) -> Result<usize, String> {
    if buffer.len() < 6 {
        return Err("hdhomerun packet buffer too small".to_owned());
    }
    buffer[0..2].copy_from_slice(&GET_SET_REQUEST.to_be_bytes());
    // Bytes 2..4 are the payload length, written by `finish_packet`.
    let mut offset = 4;
    buffer[offset] = GET_SET_NAME;
    offset += 1;
    offset += write_null_terminated_string(&mut buffer[offset..], payload)?;
    Ok(offset)
}

/// `HdHomerunManager.FinishPacket` (v10.11.8 HdHomerunManager.cs:272-282): back-fill
/// the big-endian payload length, then append the little-endian CRC-32 of
/// everything before it.
fn finish_packet(buffer: &mut [u8], offset: usize) -> Result<usize, String> {
    if buffer.len() < offset + 4 {
        return Err("hdhomerun packet buffer too small".to_owned());
    }
    let payload_len = u16::try_from(offset - 4)
        .map_err(|_| "hdhomerun payload exceeds a two-byte length".to_owned())?;
    buffer[2..4].copy_from_slice(&payload_len.to_be_bytes());
    let crc = crc32(&buffer[..offset]);
    buffer[offset..offset + 4].copy_from_slice(&crc.to_le_bytes());
    Ok(offset + 4)
}

/// `HdHomerunManager.WriteGetMessage` (v10.11.8 HdHomerunManager.cs:225-230) — read
/// `"/tuner{tuner}/{name}"`.
///
/// # Errors
///
/// Fails when the message does not fit `buffer`.
pub fn write_get_message(buffer: &mut [u8], tuner: i32, name: &str) -> Result<usize, String> {
    let offset = write_header_and_payload(buffer, &format!("/tuner{tuner}/{name}"))?;
    finish_packet(buffer, offset)
}

/// `HdHomerunManager.WriteSetMessage` (v10.11.8 HdHomerunManager.cs:232-249) — write
/// `"/tuner{tuner}/{name}" = value`, optionally under a lock key.
///
/// # Errors
///
/// Fails when the message does not fit `buffer`.
pub fn write_set_message(
    buffer: &mut [u8],
    tuner: i32,
    name: &str,
    value: &str,
    lockkey: Option<u32>,
) -> Result<usize, String> {
    let mut offset = write_header_and_payload(buffer, &format!("/tuner{tuner}/{name}"))?;
    if buffer.len() <= offset {
        return Err("hdhomerun packet buffer too small".to_owned());
    }
    buffer[offset] = GET_SET_VALUE;
    offset += 1;
    offset += write_null_terminated_string(&mut buffer[offset..], value)?;

    if let Some(lockkey) = lockkey {
        if buffer.len() < offset + 6 {
            return Err("hdhomerun packet buffer too small".to_owned());
        }
        buffer[offset] = GET_SET_LOCKKEY;
        offset += 1;
        buffer[offset] = 4;
        offset += 1;
        buffer[offset..offset + 4].copy_from_slice(&lockkey.to_be_bytes());
        offset += 4;
    }

    finish_packet(buffer, offset)
}

/// `HdHomerunManager.TryGetReturnValueOfGetSet` (v10.11.8 HdHomerunManager.cs:295-345)
/// — the value out of a get/set reply, or `None` when any framing check fails.
///
/// Every rejection below is one of upstream's, in upstream's order: too short,
/// bad CRC, wrong packet type, a declared message length that does not match
/// the buffer, a missing name tag, a name length that overruns, a missing value
/// tag, a value length that overruns.
#[must_use]
pub fn try_get_return_value_of_get_set(buffer: &[u8]) -> Option<&[u8]> {
    if buffer.len() < 8 {
        return None;
    }
    let split = buffer.len() - 4;
    let crc = u32::from_le_bytes(buffer[split..].try_into().ok()?);
    if crc != crc32(&buffer[..split]) {
        return None;
    }
    if u16::from_be_bytes(buffer[0..2].try_into().ok()?) != GET_SET_REPLY {
        return None;
    }
    let msg_length = usize::from(u16::from_be_bytes(buffer[2..4].try_into().ok()?));
    if buffer.len() != 2 + 2 + 4 + msg_length {
        return None;
    }

    let mut offset = 4;
    if buffer[offset] != GET_SET_NAME {
        return None;
    }
    offset += 1;
    let name_length = usize::from(buffer[offset]);
    offset += 1;
    if buffer.len() < 4 + 1 + offset + name_length {
        return None;
    }
    offset += name_length;

    if buffer[offset] != GET_SET_VALUE {
        return None;
    }
    offset += 1;
    let value_length = usize::from(buffer[offset]);
    offset += 1;
    if buffer.len() < 4 + offset + value_length {
        return None;
    }
    // Drop the null terminator. A zero-length value would underflow here;
    // upstream's `valueLength - 1` has the same shape, and the overrun check
    // above is what keeps a malformed packet out.
    let end = offset.checked_add(value_length.checked_sub(1)?)?;
    buffer.get(offset..end)
}

/// `HdHomerunManager.VerifyReturnValueOfGetSet` (v10.11.8 HdHomerunManager.cs:289-293)
/// — the reply's value equals `expected`, case-insensitively.
#[must_use]
pub fn verify_return_value_of_get_set(buffer: &[u8], expected: &str) -> bool {
    try_get_return_value_of_get_set(buffer)
        .and_then(|value| std::str::from_utf8(value).ok())
        .is_some_and(|value| value.eq_ignore_ascii_case(expected))
}

/// The size of the scratch buffer every exchange uses.
///
/// `ArrayPool<byte>.Shared.Rent(8192)` at each of upstream's five call sites
/// (HdHomerunManager.cs:60, 92, 163, 199). It bounds one control packet, not a
/// media stream, and is a ported constant rather than a tuning knob.
const PACKET_BUFFER: usize = 8192;

/// A live control session with one device.
///
/// Port of the stateful half of `HdHomerunManager`: the TCP connection, the
/// lock key it claimed and the tuner index it claimed it on.
#[derive(Debug)]
pub struct HdHomerunSession {
    /// The connected control socket.
    stream: TcpStream,
    /// `HdHomerunManager._lockkey` — the random key this session holds.
    lockkey: u32,
    /// `HdHomerunManager._activeTuner` — the tuner index that was claimed.
    active_tuner: i32,
}

impl HdHomerunSession {
    /// `HdHomerunManager.StartStreaming` (v10.11.8 HdHomerunManager.cs:74-152):
    /// connect, mint a lock key, then walk the tuners until one is free,
    /// claiming it, running the channel commands and pointing its `target` at
    /// `rtp://{local_ip}:{local_port}`.
    ///
    /// # Errors
    ///
    /// Fails when the device cannot be reached, or when no tuner is available
    /// (upstream's `LiveTvConflictException("No tuners available")`).
    pub async fn start_streaming(
        device: SocketAddr,
        local_ip: IpAddr,
        local_port: u16,
        commands: &[ChannelCommand],
        num_tuners: i32,
        lockkey: u32,
    ) -> Result<Self, ServiceError> {
        let mut stream = TcpStream::connect(device)
            .await
            .map_err(|e| LiveTvError::io(format!("connect to hdhomerun {device}"), e))?;
        let mut buffer = vec![0_u8; PACKET_BUFFER];

        for tuner in 0..num_tuners {
            if !check_tuner_availability(&mut stream, tuner, &mut buffer).await? {
                continue;
            }
            let mut session = Self {
                stream,
                lockkey,
                active_tuner: tuner,
            };

            // Claim it. `{0:d}` on a uint is the plain decimal spelling.
            let claimed = session
                .exchange(&mut buffer, tuner, "lockkey", &lockkey.to_string(), None)
                .await?;
            if !claimed {
                stream = session.stream;
                continue;
            }

            for (name, value) in commands {
                let ok = session
                    .exchange(&mut buffer, tuner, name, value, Some(lockkey))
                    .await?;
                if !ok {
                    // Upstream releases the key and keeps going through the
                    // remaining commands (HdHomerunManager.cs:114-121).
                    session.release_lockkey(&mut buffer).await;
                }
            }

            let target = format!("rtp://{local_ip}:{local_port}");
            let targeted = session
                .exchange(&mut buffer, tuner, "target", &target, Some(lockkey))
                .await?;
            if targeted {
                return Ok(session);
            }
            session.release_lockkey(&mut buffer).await;
            stream = session.stream;
        }

        Err(ServiceError::Conflict("No tuners available".to_owned()))
    }

    /// One set-and-check round trip: write the message, read the reply, and say
    /// whether it parsed.
    async fn exchange(
        &mut self,
        buffer: &mut [u8],
        tuner: i32,
        name: &str,
        value: &str,
        lockkey: Option<u32>,
    ) -> Result<bool, ServiceError> {
        let len = write_set_message(buffer, tuner, name, value, lockkey)
            .map_err(ServiceError::backend)?;
        self.stream
            .write_all(&buffer[..len])
            .await
            .map_err(|e| LiveTvError::io(format!("hdhomerun set /tuner{tuner}/{name}"), e))?;
        let received = self
            .stream
            .read(buffer)
            .await
            .map_err(|e| LiveTvError::io(format!("hdhomerun reply /tuner{tuner}/{name}"), e))?;
        Ok(try_get_return_value_of_get_set(&buffer[..received]).is_some())
    }

    /// `HdHomerunManager.ReleaseLockkey` (v10.11.8 HdHomerunManager.cs:196-218):
    /// clear the target, then drop the key. Errors are swallowed — this runs on
    /// the teardown path, where upstream's `Dispose` discards them too.
    async fn release_lockkey(&mut self, buffer: &mut [u8]) {
        let tuner = self.active_tuner;
        let key = self.lockkey;
        for (name, value) in [("target", "none"), ("lockkey", "none")] {
            let Ok(len) = write_set_message(buffer, tuner, name, value, Some(key)) else {
                return;
            };
            if self.stream.write_all(&buffer[..len]).await.is_err() {
                return;
            }
            if self.stream.read(buffer).await.is_err() {
                return;
            }
        }
    }

    /// `HdHomerunManager.StopStreaming`/`Dispose` — release the lock key so the
    /// tuner is free for the next viewer.
    pub async fn stop_streaming(mut self) {
        let mut buffer = vec![0_u8; PACKET_BUFFER];
        self.release_lockkey(&mut buffer).await;
    }
}

/// `HdHomerunManager.CheckTunerAvailability` (v10.11.8 HdHomerunManager.cs:57-72):
/// a tuner is free exactly when its `lockkey` reads back as `"none"`.
async fn check_tuner_availability(
    stream: &mut TcpStream,
    tuner: i32,
    buffer: &mut [u8],
) -> Result<bool, ServiceError> {
    let len = write_get_message(buffer, tuner, "lockkey").map_err(ServiceError::backend)?;
    stream
        .write_all(&buffer[..len])
        .await
        .map_err(|e| LiveTvError::io(format!("hdhomerun get /tuner{tuner}/lockkey"), e))?;
    let received = stream
        .read(buffer)
        .await
        .map_err(|e| LiveTvError::io(format!("hdhomerun reply /tuner{tuner}/lockkey"), e))?;
    Ok(verify_return_value_of_get_set(&buffer[..received], "none"))
}

#[cfg(test)]
mod tests {
    use super::{
        channel_commands, crc32, legacy_channel_commands, try_get_return_value_of_get_set,
        verify_return_value_of_get_set, write_get_message, write_null_terminated_string,
        write_set_message,
    };
    use rstest::rstest;

    /// `Crc32Tests` (v10.11.8 tests/Jellyfin.Common.Tests/Crc32Tests.cs),
    /// transliterated — the checksum the whole protocol rests on.
    #[rstest]
    #[case("", 0x0000_0000)]
    #[case(
        "54686520717569636B2062726F776E20666F78206A756D7073206F76657220746865206C617A7920646F67",
        0x414f_a339
    )]
    #[case(
        "0000000000000000000000000000000000000000000000000000000000000000",
        0x190a_55ad
    )]
    #[case(
        "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
        0xff6c_ab0b
    )]
    #[case(
        "000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F",
        0x9126_7e8a
    )]
    fn crc32_matches_the_upstream_vectors(#[case] hex: &str, #[case] expected: u32) {
        let bytes: Vec<u8> = (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("hex"))
            .collect();
        assert_eq!(crc32(&bytes), expected);
    }

    fn written(f: impl FnOnce(&mut [u8]) -> Result<usize, String>) -> Vec<u8> {
        let mut buffer = [0_u8; 128];
        let len = f(&mut buffer).expect("writes");
        buffer[..len].to_vec()
    }

    // ---- HdHomerunManagerTests.cs, all 11 facts, transliterated -------------

    #[test]
    fn write_null_terminated_string_empty_success() {
        assert_eq!(written(|b| write_null_terminated_string(b, "")), vec![1, 0]);
    }

    #[test]
    fn write_null_terminated_string_valid_success() {
        assert_eq!(
            written(|b| write_null_terminated_string(b, "The quick")),
            vec![10, b'T', b'h', b'e', b' ', b'q', b'u', b'i', b'c', b'k', 0]
        );
    }

    #[test]
    fn write_get_message_valid_success() {
        assert_eq!(
            written(|b| write_get_message(b, 0, "N")),
            vec![
                0, 4, //
                0, 12, //
                3,  //
                10, b'/', b't', b'u', b'n', b'e', b'r', b'0', b'/', b'N', 0, //
                0xc0, 0xc9, 0x87, 0x33,
            ]
        );
    }

    #[test]
    fn write_set_message_no_lock_key_success() {
        assert_eq!(
            written(|b| write_set_message(b, 0, "N", "value", None)),
            vec![
                0, 4, //
                0, 20, //
                3,  //
                10, b'/', b't', b'u', b'n', b'e', b'r', b'0', b'/', b'N', 0, //
                4, //
                6, b'v', b'a', b'l', b'u', b'e', 0, //
                0xa9, 0x49, 0xd0, 0x68,
            ]
        );
    }

    #[test]
    fn write_set_message_lock_key_success() {
        assert_eq!(
            written(|b| write_set_message(b, 0, "N", "value", Some(80085))),
            vec![
                0, 4, //
                0, 26, //
                3,  //
                10, b'/', b't', b'u', b'n', b'e', b'r', b'0', b'/', b'N', 0, //
                4, //
                6, b'v', b'a', b'l', b'u', b'e', 0,  //
                21, //
                4, 0x00, 0x01, 0x38, 0xd5, //
                0x8e, 0xb6, 0x06, 0x82,
            ]
        );
    }

    /// The valid reply every negative case below is a one-field mutation of.
    fn valid_reply() -> Vec<u8> {
        vec![
            0, 5, //
            0, 20, //
            3,  //
            10, b'/', b't', b'u', b'n', b'e', b'r', b'0', b'/', b'N', 0, //
            4, //
            6, b'v', b'a', b'l', b'u', b'e', 0, //
            0x7d, 0xa3, 0xa3, 0xf3,
        ]
    }

    #[test]
    fn try_get_return_value_of_get_set_valid_success() {
        let reply = valid_reply();
        let value = try_get_return_value_of_get_set(&reply).expect("parses");
        assert_eq!(std::str::from_utf8(value).expect("utf8"), "value");
    }

    #[rstest]
    // `TryGetReturnValueOfGetSet_InvalidCrc_False`
    #[case(vec![0,5, 0,20, 3, 10,b'/',b't',b'u',b'n',b'e',b'r',b'0',b'/',b'N',0, 4, 6,b'v',b'a',b'l',b'u',b'e',0, 0x7d,0xa3,0xa3,0xf4])]
    // `TryGetReturnValueOfGetSet_InvalidPacketType_False`
    #[case(vec![0,4, 0,20, 3, 10,b'/',b't',b'u',b'n',b'e',b'r',b'0',b'/',b'N',0, 4, 6,b'v',b'a',b'l',b'u',b'e',0, 0xa9,0x49,0xd0,0x68])]
    // `TryGetReturnValueOfGetSet_InvalidPacket_False`
    #[case(vec![0,5, 0,20, 0x7d,0xa3,0xa3])]
    // `TryGetReturnValueOfGetSet_TooSmallMessageLength_False`
    #[case(vec![0,5, 0,19, 3, 10,b'/',b't',b'u',b'n',b'e',b'r',b'0',b'/',b'N',0, 4, 6,b'v',b'a',b'l',b'u',b'e',0, 0x25,0x25,0x44,0x9a])]
    // `TryGetReturnValueOfGetSet_TooLargeMessageLength_False`
    #[case(vec![0,5, 0,21, 3, 10,b'/',b't',b'u',b'n',b'e',b'r',b'0',b'/',b'N',0, 4, 6,b'v',b'a',b'l',b'u',b'e',0, 0xe3,0x20,0x79,0x6c])]
    // `TryGetReturnValueOfGetSet_TooLargeNameLength_False`
    #[case(vec![0,5, 0,20, 3, 20,b'/',b't',b'u',b'n',b'e',b'r',b'0',b'/',b'N',0, 4, 6,b'v',b'a',b'l',b'u',b'e',0, 0xe1,0x8e,0x9c,0x74])]
    // `TryGetReturnValueOfGetSet_InvalidGetSetNameTag_False`
    #[case(vec![0,5, 0,20, 4, 10,b'/',b't',b'u',b'n',b'e',b'r',b'0',b'/',b'N',0, 4, 6,b'v',b'a',b'l',b'u',b'e',0, 0xee,0x05,0xe7,0x12])]
    // `TryGetReturnValueOfGetSet_InvalidGetSetValueTag_False`
    #[case(vec![0,5, 0,20, 3, 10,b'/',b't',b'u',b'n',b'e',b'r',b'0',b'/',b'N',0, 3, 6,b'v',b'a',b'l',b'u',b'e',0, 0x64,0xaa,0x66,0xf9])]
    // `TryGetReturnValueOfGetSet_TooLargeValueLength_False`
    #[case(vec![0,5, 0,20, 3, 10,b'/',b't',b'u',b'n',b'e',b'r',b'0',b'/',b'N',0, 4, 7,b'v',b'a',b'l',b'u',b'e',0, 0xc9,0xa8,0xd4,0x55])]
    fn try_get_return_value_of_get_set_rejects_a_malformed_packet(#[case] packet: Vec<u8>) {
        assert!(try_get_return_value_of_get_set(&packet).is_none());
    }

    #[test]
    fn verify_return_value_of_get_set_valid_true() {
        assert!(verify_return_value_of_get_set(&valid_reply(), "value"));
    }

    #[test]
    fn verify_return_value_of_get_set_wrong_value_false() {
        assert!(!verify_return_value_of_get_set(&valid_reply(), "none"));
    }

    #[test]
    fn verify_return_value_of_get_set_invalid_packet_false() {
        let mut packet = valid_reply();
        // `VerifyReturnValueOfGetSet_InvalidPacket_False` flips the packet type
        // to a REQUEST while keeping the reply's CRC.
        packet[1] = 4;
        assert!(!verify_return_value_of_get_set(&packet, "value"));
    }

    // ---- the two channel-command shapes ------------------------------------

    #[rstest]
    #[case(Some("4.1"), None, vec![("vchannel".to_owned(), "4.1".to_owned())])]
    #[case(Some("4.1"), Some("native"), vec![("vchannel".to_owned(), "4.1".to_owned())])]
    #[case(Some("4.1"), Some("NATIVE"), vec![("vchannel".to_owned(), "4.1".to_owned())])]
    #[case(Some("4.1"), Some(""), vec![("vchannel".to_owned(), "4.1".to_owned())])]
    #[case(Some("4.1"), Some("heavy"), vec![("vchannel".to_owned(), "4.1 transcode=heavy".to_owned())])]
    #[case(None, Some("heavy"), vec![])]
    #[case(Some(""), Some("heavy"), vec![])]
    fn channel_commands_match_the_upstream_yield(
        #[case] channel: Option<&str>,
        #[case] profile: Option<&str>,
        #[case] expected: Vec<(String, String)>,
    ) {
        assert_eq!(channel_commands(channel, profile), expected);
    }

    #[rstest]
    // `@"\/ch([0-9]+)-?([0-9]*)"` — channel, then an optional program.
    #[case("hdhomerun://1020304A-0/ch4-1", vec![("channel".to_owned(), "4".to_owned()), ("program".to_owned(), "1".to_owned())])]
    #[case("hdhomerun://1020304A-0/ch12", vec![("channel".to_owned(), "12".to_owned())])]
    #[case("hdhomerun://1020304A-0/ch12-", vec![("channel".to_owned(), "12".to_owned())])]
    #[case("hdhomerun://1020304A-0/tuner0", vec![])]
    #[case("", vec![])]
    // `/ch` with no digits is not a match, and `Regex.Match` keeps scanning.
    #[case("hdhomerun://x/chan/ch7-3", vec![("channel".to_owned(), "7".to_owned()), ("program".to_owned(), "3".to_owned())])]
    fn legacy_channel_commands_parse_the_device_url(
        #[case] url: &str,
        #[case] expected: Vec<(String, String)>,
    ) {
        assert_eq!(legacy_channel_commands(url), expected);
    }

    #[test]
    fn an_oversized_payload_is_rejected_rather_than_truncated() {
        // Upstream's `Convert.ToByte(len)` throws here; the port must not
        // silently write a wrong length byte.
        let mut buffer = [0_u8; 512];
        assert!(write_null_terminated_string(&mut buffer, &"x".repeat(255)).is_err());
        // …and a buffer that cannot hold the message fails rather than panics.
        let mut tiny = [0_u8; 4];
        assert!(write_get_message(&mut tiny, 0, "N").is_err());
        let mut small = [0_u8; 20];
        assert!(write_set_message(&mut small, 0, "N", "value", Some(1)).is_err());
    }
}
