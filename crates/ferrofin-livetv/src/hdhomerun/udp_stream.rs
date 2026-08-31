//! The legacy HDHomeRun UDP stream.
//!
//! Port of `HdHomerunUdpStream.cs` (v10.11.8
//! `src/Jellyfin.LiveTv/TunerHosts/HdHomerun/HdHomerunUdpStream.cs`).
//!
//! A pre-`vchannel` HDHomeRun does not serve HTTP per channel. It is told —
//! over the [control protocol](super::manager) — to send the tuned channel as
//! RTP to an address this server names, so the server first binds a UDP socket,
//! then discovers which of its own addresses the device can reach by opening a
//! throwaway TCP connection to it, and finally pumps the datagrams (minus their
//! 12-byte RTP headers) into the same shared temp file every other live stream
//! uses. The opened media source therefore looks exactly like a shared HTTP
//! stream to the rest of the server: a `/LiveTv/LiveStreamFiles/{id}/stream.ts`
//! URL over `MediaProtocol.Http`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::Utc;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UdpSocket};

use ferrofin_traits::error::ServiceError;

use crate::error::LiveTvError;
use crate::hdhomerun::host::LegacyUdpPlan;
use crate::hdhomerun::manager::HdHomerunSession;
use crate::stream::OpenedSharedStream;

/// `HdHomerunUdpStream.RtpHeaderBytes` (v10.11.8 HdHomerunUdpStream.cs:25) — the
/// fixed RTP header the device prepends to every datagram, stripped before the
/// payload is written.
const RTP_HEADER_BYTES: usize = 12;

/// The ephemeral range `GetUdpPortFromRange((49152, 65535))` picks from
/// (v10.11.8 HdHomerunUdpStream.cs:80). IANA's dynamic/private port range; the
/// number is upstream's, not a Ferrofin knob.
const LOCAL_PORT_RANGE: std::ops::RangeInclusive<u16> = 49152..=65535;

/// The largest datagram the receive buffer holds. An MPEG-TS-over-RTP packet is
/// 7 × 188 + 12 = 1328 bytes, so a 64 KiB buffer cannot truncate one.
const RECEIVE_BUFFER: usize = 65_536;

/// How long the receive loop waits for a datagram before giving up.
///
/// `WaitAsync(TimeSpan.FromMilliseconds(30000))` on each receive
/// (v10.11.8 HdHomerunUdpStream.cs:195) — a device that has gone quiet for
/// thirty seconds has stopped, and upstream ends the stream rather than
/// hanging on it.
const RECEIVE_TIMEOUT_SECONDS: u64 = 30;

/// How long the first datagram may take before the open is called a failure —
/// the same budget the shared HTTP path gives a tuner's first byte.
const FIRST_PACKET_TIMEOUT_SECONDS: u64 = 30;

/// Binds a UDP socket on a free port inside upstream's range.
///
/// `GetUdpPortFromRange` enumerates the active UDP listeners and takes the
/// first unused number; binding and letting the OS reject a taken port is the
/// same search without the race between the enumeration and the bind.
async fn bind_local_socket() -> Result<UdpSocket, ServiceError> {
    for port in LOCAL_PORT_RANGE {
        if let Ok(socket) = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))).await {
            return Ok(socket);
        }
    }
    Err(ServiceError::backend(
        "no free UDP port in 49152-65535 for the hdhomerun stream",
    ))
}

/// The local address the device can reach us on.
///
/// Port of HdHomerunUdpStream.cs:88-105: open a throwaway TCP connection to the
/// device's control port and read the local end of it, which is the address the
/// routing table picked for that device — then unmap an IPv4-mapped IPv6.
async fn local_address_towards(device: SocketAddr) -> Result<IpAddr, ServiceError> {
    let probe = TcpStream::connect(device).await.map_err(|e| {
        LiveTvError::io(
            format!("determine the local address for hdhomerun {device}"),
            e,
        )
    })?;
    let local = probe
        .local_addr()
        .map_err(|e| LiveTvError::io("read the local address of the hdhomerun probe", e))?
        .ip();
    drop(probe);
    Ok(match local {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(local, IpAddr::V4),
        v4 @ IpAddr::V4(_) => v4,
    })
}

/// Opens a legacy HDHomeRun stream: claim a tuner, point it at us, and start
/// buffering the RTP payload into `{transcode_dir}/{uniqueId}.ts`.
///
/// Port of `HdHomerunUdpStream.Open` + `StartStreaming` + `CopyTo`. Resolves
/// once the first datagram has landed, exactly as the shared HTTP path resolves
/// on the first byte, so a device that accepts the control commands but never
/// sends media is an error rather than a silent empty file.
///
/// # Errors
///
/// Fails when no local port is free, the device is unreachable, no tuner is
/// available, or the device sent nothing at all.
pub async fn open_legacy_udp_stream(
    plan: &LegacyUdpPlan,
    transcode_dir: &Path,
    lockkey: u32,
) -> Result<OpenedSharedStream, ServiceError> {
    let unique_id = uuid::Uuid::new_v4().simple().to_string();
    let temp_path = transcode_dir.join(format!("{unique_id}.ts"));
    tokio::fs::create_dir_all(transcode_dir)
        .await
        .map_err(|e| LiveTvError::io(format!("create {}", transcode_dir.display()), e))?;

    let socket = bind_local_socket().await?;
    let local_port = socket
        .local_addr()
        .map_err(|e| LiveTvError::io("read the hdhomerun receive port", e))?
        .port();
    let local_ip = local_address_towards(plan.device).await?;

    tracing::info!(device = %plan.device, local_port, "live tv: opening an hdhomerun udp live stream");

    // Only once the tuner is claimed and pointed at us does anything arrive.
    let session = HdHomerunSession::start_streaming(
        plan.device,
        local_ip,
        local_port,
        &plan.commands,
        plan.num_tuners,
        lockkey,
    )
    .await?;

    let alive = Arc::new(AtomicBool::new(true));
    let (first_packet_tx, first_packet_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(copy_datagrams_to_temp_file(
        socket,
        session,
        temp_path.clone(),
        first_packet_tx,
        Arc::clone(&alive),
    ));

    let first = tokio::time::timeout(
        std::time::Duration::from_secs(FIRST_PACKET_TIMEOUT_SECONDS),
        first_packet_rx,
    )
    .await;
    if let Ok(Ok(true)) = first {
        Ok(OpenedSharedStream {
            unique_id,
            temp_path,
            opened_at: Utc::now(),
            alive,
            task,
        })
    } else {
        task.abort();
        let _ = tokio::fs::remove_file(&temp_path).await;
        Err(ServiceError::backend(format!(
            "zero bytes received from hdhomerun {}",
            plan.device
        )))
    }
}

/// The receive loop: strip each datagram's RTP header, append the payload, and
/// release the device's tuner when it ends.
async fn copy_datagrams_to_temp_file(
    socket: UdpSocket,
    session: HdHomerunSession,
    temp_path: PathBuf,
    first_packet: tokio::sync::oneshot::Sender<bool>,
    alive: Arc<AtomicBool>,
) {
    let mut first_packet = Some(first_packet);
    let mut file = match tokio::fs::File::create(&temp_path).await {
        Ok(file) => file,
        Err(error) => {
            tracing::error!(path = %temp_path.display(), %error, "live tv: opening the hdhomerun buffer failed");
            if let Some(tx) = first_packet.take() {
                let _ = tx.send(false);
            }
            // The tuner was claimed before this ran, so it must be released
            // even though not one byte was written.
            session.stop_streaming().await;
            return;
        }
    };

    let mut buffer = vec![0_u8; RECEIVE_BUFFER];
    loop {
        let received = tokio::time::timeout(
            std::time::Duration::from_secs(RECEIVE_TIMEOUT_SECONDS),
            socket.recv(&mut buffer),
        )
        .await;
        let Ok(Ok(received)) = received else {
            // A timeout or a socket error ends the stream, as upstream's
            // `catch (ex is OperationCanceledException || TimeoutException)`
            // does.
            break;
        };
        if received > RTP_HEADER_BYTES
            && file
                .write_all(&buffer[RTP_HEADER_BYTES..received])
                .await
                .is_err()
        {
            break;
        }
        if let Some(tx) = first_packet.take() {
            let _ = tx.send(true);
        }
    }

    // Whatever ended the loop, the buffer stops growing here, so the stream
    // stops being joinable at the same moment (the shared HTTP path's
    // `SharingGuard`).
    alive.store(false, Ordering::SeqCst);
    if let Some(tx) = first_packet.take() {
        let _ = tx.send(false);
    }
    let _ = file.flush().await;
    session.stop_streaming().await;
    let _ = tokio::fs::remove_file(&temp_path).await;
}

#[cfg(test)]
mod tests {
    use super::{
        RTP_HEADER_BYTES, bind_local_socket, local_address_towards, open_legacy_udp_stream,
    };
    use crate::hdhomerun::host::LegacyUdpPlan;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    #[tokio::test]
    async fn the_receive_socket_lands_in_upstreams_port_range() {
        let socket = bind_local_socket().await.expect("bind");
        let port = socket.local_addr().expect("addr").port();
        assert!(
            (49152..=65535).contains(&port),
            "picked {port}, outside GetUdpPortFromRange((49152, 65535))"
        );
    }

    #[tokio::test]
    async fn the_local_address_probe_reports_the_route_to_the_device() {
        // A listener on loopback stands in for the device's control port; the
        // route to 127.0.0.1 is 127.0.0.1, which is what the probe must read.
        let device = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listen");
        let addr = device.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = device.accept().await;
        });
        assert_eq!(
            local_address_towards(addr).await.expect("probe"),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
    }

    #[tokio::test]
    async fn an_unreachable_device_fails_the_local_address_probe() {
        // Port 1 on loopback: connection refused, which upstream logs and
        // returns from `Open` on.
        let err = local_address_towards(SocketAddr::from((Ipv4Addr::LOCALHOST, 1)))
            .await
            .expect_err("must fail");
        assert!(err.to_string().contains("local address"), "{err}");
    }

    #[tokio::test]
    async fn an_unreachable_device_fails_the_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plan = LegacyUdpPlan {
            device: SocketAddr::from((Ipv4Addr::LOCALHOST, 1)),
            num_tuners: 2,
            commands: vec![("channel".to_owned(), "4".to_owned())],
        };
        assert!(
            open_legacy_udp_stream(&plan, dir.path(), 1234)
                .await
                .is_err(),
            "nothing is listening on the control port"
        );
    }

    /// The whole legacy path against the [fake device](crate::hdhomerun::fake):
    /// claim a tuner, run the channel commands, take the `target` the device is
    /// told to send to, and stream RTP at it. What lands in the buffer must be
    /// the payload with every 12-byte RTP header stripped.
    #[tokio::test]
    async fn the_legacy_path_tunes_a_fake_device_and_buffers_its_rtp() {
        let device = crate::hdhomerun::fake::FakeDevice::start(2).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let plan = LegacyUdpPlan {
            device: device.addr,
            num_tuners: 2,
            commands: crate::hdhomerun::manager::legacy_channel_commands(
                "hdhomerun://1040A0A1-0/ch4-1",
            ),
        };

        let opened = open_legacy_udp_stream(&plan, dir.path(), 0x1234_5678)
            .await
            .expect("the device accepted the tune and started sending");

        // The device saw the exact commands `LegacyHdHomerunChannelCommands`
        // yields for that URL, then the RTP target — each under the lock key.
        let seen = device.commands();
        assert_eq!(
            seen.iter()
                .map(|(n, v, _)| (n.as_str(), v.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("lockkey", "305419896"),
                ("channel", "4"),
                ("program", "1"),
                ("target", seen[3].1.as_str()),
            ]
        );
        assert!(seen[3].1.starts_with("rtp://127.0.0.1:"), "{}", seen[3].1);
        // The channel/program/target commands carry the lock key; claiming it
        // does not (there is nothing to authorise yet).
        assert_eq!(
            seen.iter().map(|(_, _, k)| *k).collect::<Vec<_>>(),
            vec![
                None,
                Some(0x1234_5678),
                Some(0x1234_5678),
                Some(0x1234_5678)
            ]
        );

        // …and the RTP headers are gone from what a consumer will read.
        for _ in 0..40 {
            let buffered = tokio::fs::read(&opened.temp_path).await.unwrap_or_default();
            if buffered.len() >= crate::hdhomerun::fake::PAYLOAD.len() * 2 {
                assert!(
                    buffered.starts_with(crate::hdhomerun::fake::PAYLOAD),
                    "the {RTP_HEADER_BYTES}-byte RTP header must be stripped"
                );
                opened.task.abort();
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        opened.task.abort();
        panic!("the buffer never filled");
    }

    /// Every tuner busy is upstream's `LiveTvConflictException("No tuners
    /// available")`, not a silent empty stream.
    #[tokio::test]
    async fn a_device_with_no_free_tuner_is_a_conflict() {
        let device = crate::hdhomerun::fake::FakeDevice::start(0).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let plan = LegacyUdpPlan {
            device: device.addr,
            num_tuners: 2,
            commands: vec![("channel".to_owned(), "4".to_owned())],
        };
        let err = open_legacy_udp_stream(&plan, dir.path(), 7)
            .await
            .expect_err("no tuner is free");
        assert!(
            matches!(err, ferrofin_traits::error::ServiceError::Conflict(ref m)
                     if m == "No tuners available"),
            "{err}"
        );
    }
}
