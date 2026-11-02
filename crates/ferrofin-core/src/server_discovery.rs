//! Jellyfin-compatible UDP LAN discovery (`Jellyfin.Networking.AutoDiscoveryHost`).
//!
//! The composition root decides whether to bind and owns task cancellation. Abort
//! and await the task before starting another lifetime, so its socket is released.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use ferrofin_model::api_client::ServerDiscoveryInfo;
use ferrofin_traits::system::ServerApplicationHost;
use tokio::net::UdpSocket;
use tracing::Instrument;

/// Failure opening or inspecting the discovery socket.
#[derive(Debug, thiserror::Error)]
pub enum ServerDiscoveryError {
    /// The operating system rejected a UDP socket operation.
    #[error("discovery socket: {0}")]
    Socket(#[from] io::Error),
}

/// Responds to Jellyfin discovery requests using the application's current facts.
pub struct ServerDiscovery {
    socket: UdpSocket,
    host: Arc<dyn ServerApplicationHost>,
    server_id: String,
}

impl ServerDiscovery {
    /// Binds the responder; production uses `0.0.0.0:7359`.
    ///
    /// Tests can use `127.0.0.1:0` and query [`Self::local_addr`] before spawning.
    ///
    /// # Errors
    /// Returns the socket error if the address cannot be bound.
    pub async fn bind(
        bind_addr: SocketAddr,
        host: Arc<dyn ServerApplicationHost>,
        server_id: String,
    ) -> Result<Self, ServerDiscoveryError> {
        Ok(Self {
            socket: UdpSocket::bind(bind_addr).await?,
            host,
            server_id,
        })
    }

    /// Returns the bound address, including the OS-assigned port when binding zero.
    ///
    /// # Errors
    /// Returns an error if the operating system cannot inspect the socket.
    pub fn local_addr(&self) -> Result<SocketAddr, ServerDiscoveryError> {
        Ok(self.socket.local_addr()?)
    }

    /// Runs until aborted or a terminal response-construction error occurs.
    ///
    /// Socket errors are logged and the next request is attempted, matching
    /// upstream. This future owns its socket; await the aborted task before rebind.
    pub async fn run(self) {
        self.listen()
            .instrument(tracing::info_span!(parent: None, "server_discovery", task = "server_discovery", trigger = "startup"))
            .await;
    }

    async fn listen(self) {
        // A UDP length is a u16 including its header. This also accommodates the
        // largest IPv4 payload (65,507 bytes) without truncating containing text.
        let mut buffer = vec![0_u8; usize::from(u16::MAX)];
        loop {
            let (length, peer) = match self.socket.recv_from(&mut buffer).await {
                Ok(received) => received,
                Err(error) => {
                    tracing::error!(%error, "Failed to receive server discovery datagram");
                    continue;
                }
            };
            let request = String::from_utf8_lossy(&buffer[..length]);
            if !request
                .to_ascii_lowercase()
                .contains("who is jellyfinserver?")
            {
                continue;
            }

            let address = match self.host.get_smart_api_url_for_peer(peer.ip()).await {
                Ok(address) => address,
                Err(error) => {
                    tracing::error!(%error, %peer, "Unable to resolve server discovery address");
                    return;
                }
            };
            if address.is_empty() {
                tracing::warn!(%peer, "Unable to respond to server discovery: no local API address");
                continue;
            }
            let response = ServerDiscoveryInfo {
                address,
                id: self.server_id.clone(),
                name: self.host.friendly_name(),
                endpoint_address: None,
            };
            let payload = match serde_json::to_vec(&response) {
                Ok(payload) => payload,
                Err(error) => {
                    tracing::error!(%error, "Unable to serialize server discovery response");
                    return;
                }
            };
            tracing::debug!(%peer, "Sending server discovery response");
            if let Err(error) = self.socket.send_to(&payload, peer).await {
                tracing::error!(%error, %peer, "Failed to send server discovery response");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ferrofin_traits::configuration::ServerConfigurationManager;
    use tokio::task::JoinHandle;
    use tokio::time::timeout;

    use super::*;
    use crate::app_paths::test_paths;
    use crate::application_host::{FerrofinServerApplicationHost, HostNetworkInfo};
    use crate::configuration_manager::FerrofinServerConfigurationManager;

    struct Fixture {
        _directory: tempfile::TempDir,
        host: Arc<FerrofinServerApplicationHost>,
        configuration: Arc<FerrofinServerConfigurationManager>,
    }

    impl Fixture {
        async fn new() -> Self {
            let directory = tempfile::tempdir().expect("tempdir");
            let paths = test_paths(directory.path());
            let configuration = Arc::new(
                FerrofinServerConfigurationManager::load(Arc::clone(&paths))
                    .await
                    .expect("load config"),
            );
            let host = Arc::new(FerrofinServerApplicationHost::new(
                paths,
                configuration.clone(),
                HostNetworkInfo {
                    published_server_url: Some("http://192.168.1.2:8096/jellyfin".into()),
                    ..HostNetworkInfo::default()
                },
                "discovery-test",
            ));
            configuration.add_configuration_listener(host.configuration_listener());
            Self {
                _directory: directory,
                host,
                configuration,
            }
        }

        async fn bind(&self, address: SocketAddr) -> Result<ServerDiscovery, ServerDiscoveryError> {
            ServerDiscovery::bind(address, self.host.clone(), "persisted-id".into()).await
        }

        async fn start(&self) -> (SocketAddr, JoinHandle<()>) {
            let service = self
                .bind("127.0.0.1:0".parse().expect("loopback address"))
                .await
                .expect("bind discovery");
            let address = service.local_addr().expect("bound address");
            (address, tokio::spawn(service.run()))
        }
    }

    async fn stop(task: JoinHandle<()>) {
        task.abort();
        let error = timeout(Duration::from_secs(2), task)
            .await
            .expect("cancellation completes")
            .expect_err("task aborted");
        assert!(error.is_cancelled());
    }

    async fn client() -> UdpSocket {
        UdpSocket::bind("127.0.0.1:0").await.expect("bind client")
    }

    async fn reply(client: &UdpSocket, address: SocketAddr, request: &[u8]) -> serde_json::Value {
        client
            .send_to(request, address)
            .await
            .expect("send request");
        let mut buffer = [0_u8; 4096];
        let (length, source) = timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
            .await
            .expect("response before timeout")
            .expect("receive reply");
        assert_eq!(source, address, "reply comes from the listening socket");
        serde_json::from_slice(&buffer[..length]).expect("JSON reply")
    }

    #[tokio::test]
    async fn recognized_datagrams_reply_to_requesting_endpoint() {
        let fixture = Fixture::new().await;
        let (address, task) = fixture.start().await;
        let client = client().await;
        for request in [
            b"who is JellyfinServer?".as_slice(),
            b"prefix WHO IS jElLyFiNsErVeR? suffix".as_slice(),
            b"\xffwho is JellyfinServer?\xfe".as_slice(),
        ] {
            assert_eq!(
                reply(&client, address, request).await,
                serde_json::json!({
                    "Address": "http://192.168.1.2:8096/jellyfin",
                    "Id": "persisted-id",
                    "Name": "discovery-test",
                    "EndpointAddress": null,
                })
            );
        }
        stop(task).await;
    }

    #[tokio::test]
    async fn unrelated_and_empty_datagrams_are_silent_then_valid_request_works() {
        let fixture = Fixture::new().await;
        let (address, task) = fixture.start().await;
        let client = client().await;
        for request in [b"".as_slice(), b"who is some other server?".as_slice()] {
            client
                .send_to(request, address)
                .await
                .expect("send invalid request");
            let mut buffer = [0_u8; 4096];
            assert!(
                timeout(Duration::from_millis(50), client.recv_from(&mut buffer))
                    .await
                    .is_err()
            );
        }
        assert_eq!(
            reply(&client, address, b"who is JellyfinServer?").await["Id"],
            "persisted-id"
        );
        stop(task).await;
    }

    #[tokio::test]
    async fn maximum_ipv4_datagram_is_not_truncated() {
        let fixture = Fixture::new().await;
        let (address, task) = fixture.start().await;
        let client = client().await;
        let mut request = vec![b'x'; 65_507];
        let marker = b"who is JellyfinServer?";
        let offset = request.len() - marker.len();
        request[offset..].copy_from_slice(marker);
        assert_eq!(
            reply(&client, address, &request).await["Id"],
            "persisted-id"
        );
        stop(task).await;
    }

    #[tokio::test]
    async fn next_response_uses_saved_friendly_name_without_manual_refresh() {
        let fixture = Fixture::new().await;
        let (address, task) = fixture.start().await;
        let client = client().await;
        assert_eq!(
            reply(&client, address, b"who is JellyfinServer?").await["Name"],
            "discovery-test"
        );
        let mut configuration = fixture.configuration.snapshot();
        configuration.server_name = "New name".into();
        fixture
            .configuration
            .update_configuration(&configuration)
            .await
            .expect("save new name");
        assert_eq!(
            reply(&client, address, b"who is JellyfinServer?").await["Name"],
            "New name"
        );
        stop(task).await;
    }

    #[tokio::test]
    async fn occupied_port_is_fallible_and_abort_await_releases_socket_for_restart() {
        let fixture = Fixture::new().await;
        let (address, task) = fixture.start().await;
        assert!(
            fixture.bind(address).await.is_err(),
            "running service owns port"
        );
        let client = client().await;
        reply(&client, address, b"who is JellyfinServer?").await;
        stop(task).await;
        let service = fixture
            .bind(address)
            .await
            .expect("rebind after abort and await");
        let restarted = tokio::spawn(service.run());
        assert_eq!(
            reply(&client, address, b"who is JellyfinServer?").await["Id"],
            "persisted-id"
        );
        stop(restarted).await;
    }
}
