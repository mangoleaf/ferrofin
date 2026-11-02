//! HTTPS, end to end: with the network configuration's `EnableHttps` and a
//! PKCS#12 certificate the server listens with HTTPS on its HTTPS port and
//! publishes HTTPS addresses, `RequireHttps` redirects plain HTTP there, and a
//! new certificate path flags a restart (`ApplicationHost.ValidateSslCertificate`).

use std::time::Duration;

use ferrofin_server::config::Config;

#[path = "support/boot.rs"]
mod boot;

const ADMIN_USER: &str = "https-admin";
const ADMIN_PASSWORD: &str = "https-pw";
const CLIENT: &str =
    r#"MediaBrowser Client="https-test", Device="test", DeviceId="https-test", Version="1""#;

/// Ceiling for one request, only to turn a hang into a failure: signing in
/// runs Jellyfin's 210 000-iteration PBKDF2 in an unoptimized build (see
/// `restart.rs`).
const REQUEST_TIMEOUT: Duration = Duration::from_mins(1);

/// A self-signed certificate for `127.0.0.1`: its DER and a PKCS#12 file of it
/// and its key, encrypted with `password`.
fn self_signed_pkcs12(password: &str) -> (Vec<u8>, Vec<u8>) {
    let certified =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).expect("certificate");
    let der = certified.cert.der().to_vec();
    let chain = p12_keystore::PrivateKeyChain::new(
        "ferrofin",
        p12_keystore::PrivateKey::from_der(&certified.signing_key.serialize_der()).expect("key"),
        [p12_keystore::Certificate::from_der(&der).expect("certificate")],
    );
    let mut store = p12_keystore::KeyStore::new();
    store.add_entry(
        "ferrofin",
        p12_keystore::KeyStoreEntry::PrivateKeyChain(chain),
    );
    (der, store.writer(password).write().expect("pkcs12"))
}

fn authorization(token: &str) -> String {
    format!("{CLIENT}, Token=\"{token}\"")
}

/// A server booted with HTTPS on and required, and a client that trusts only
/// its certificate.
struct HttpsServer {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    server: std::thread::JoinHandle<anyhow::Result<()>>,
    /// The plain HTTP base URL.
    base: String,
    /// The HTTPS base URL.
    https_base: String,
    config: Config,
    /// Trusts only the configured certificate, and follows the redirect from
    /// plain HTTP, as the boot helper's polling does.
    https: reqwest::Client,
}

async fn boot_https_server() -> HttpsServer {
    boot_https_server_with(serde_json::json!({"RequireHttps": true})).await
}

/// [`boot_https_server`] with `settings` merged into the network configuration.
async fn boot_https_server_with(settings: serde_json::Value) -> HttpsServer {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let (der, pkcs12) = self_signed_pkcs12("secret");
    let certificate_path = root.join("server.pfx");
    std::fs::write(&certificate_path, pkcs12).expect("write certificate");
    let named = root.join("config").join("users").join("named");
    std::fs::create_dir_all(&named).expect("named configuration directory");
    let mut network = serde_json::json!({
        "EnableHttps": true,
        "CertificatePath": certificate_path,
        "CertificatePassword": "secret",
    });
    if let (Some(network), Some(settings)) = (network.as_object_mut(), settings.as_object()) {
        network.extend(settings.clone());
    }
    std::fs::write(named.join("network.json"), network.to_string())
        .expect("write network configuration");

    let https = reqwest::Client::builder()
        .tls_certs_only([reqwest::Certificate::from_der(&der).expect("certificate")])
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("https client");
    let (server, base, config) = boot::spawn_server(&https, |port| Config {
        server_name: "ferrofin-https".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        port,
        https_port: boot::free_port(),
        ..Config::test_stub(&root)
    })
    .await;
    HttpsServer {
        https_base: format!("https://127.0.0.1:{}", config.https_port),
        _tmp: tmp,
        root,
        server,
        base,
        config,
        https,
    }
}

impl HttpsServer {
    async fn sign_in(&self) -> String {
        let auth: serde_json::Value = self
            .https
            .post(format!("{}/Users/AuthenticateByName", self.https_base))
            .header("Authorization", CLIENT)
            .json(&serde_json::json!({"Username": ADMIN_USER, "Pw": ADMIN_PASSWORD}))
            .send()
            .await
            .expect("sign in")
            .json()
            .await
            .expect("sign-in body");
        auth["AccessToken"]
            .as_str()
            .expect("access token")
            .to_owned()
    }

    async fn get_json(&self, path: &str, token: &str) -> serde_json::Value {
        self.https
            .get(format!("{}{path}", self.https_base))
            .header("Authorization", authorization(token))
            .send()
            .await
            .expect("request")
            .json()
            .await
            .expect("json body")
    }

    async fn save_network(&self, network: &serde_json::Value, token: &str) -> reqwest::StatusCode {
        self.https
            .post(format!("{}/System/Configuration/network", self.https_base))
            .header("Authorization", authorization(token))
            .json(network)
            .send()
            .await
            .expect("save network configuration")
            .status()
    }

    async fn shut_down(self, token: &str) {
        let shutdown = self
            .https
            .post(format!("{}/System/Shutdown", self.https_base))
            .header("Authorization", authorization(token))
            .send()
            .await
            .expect("shutdown");
        assert_eq!(shutdown.status(), reqwest::StatusCode::NO_CONTENT);
        self.server
            .join()
            .expect("server thread")
            .expect("server stops cleanly");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_server_listens_with_https_and_sends_plain_http_there() {
    let server = boot_https_server().await;
    let https_base = &server.https_base;

    // HTTPS answers, with the configured certificate, and the addresses the
    // server publishes are HTTPS ones (`ListenWithHttps`).
    let info: serde_json::Value = server
        .https
        .get(format!("{https_base}/System/Info/Public"))
        .send()
        .await
        .expect("https request")
        .json()
        .await
        .expect("public info");
    assert_eq!(info["ServerName"], server.config.server_name.as_str());
    assert_eq!(info["LocalAddress"], https_base.as_str());

    // Plain HTTP is redirected to the same path and query on HTTPS; Ferrofin's
    // own probes are not.
    let plain = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("http client");
    let response = plain
        .get(format!("{}/System/Info/Public?probe=1", server.base))
        .send()
        .await
        .expect("http request");
    assert_eq!(response.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|location| location.to_str().ok()),
        Some(format!("{https_base}/System/Info/Public?probe=1").as_str())
    );
    let live = plain
        .get(format!("{}/health/live", server.base))
        .send()
        .await
        .expect("liveness probe");
    assert_eq!(live.status(), reqwest::StatusCode::OK);

    let token = server.sign_in().await;
    server.shut_down(&token).await;
}

/// `GetSmartApiUrl(HttpRequest)` with `EnablePublishedServerUriByRequest`:
/// the address takes the scheme the request came with, so a plain HTTP
/// request to a server that also serves HTTPS is answered with `http://`.
#[tokio::test(flavor = "multi_thread")]
async fn addresses_published_by_request_take_the_requests_scheme() {
    let server = boot_https_server_with(serde_json::json!({
        "RequireHttps": false,
        "EnablePublishedServerUriByRequest": true,
    }))
    .await;
    let local_address = |base: String| {
        let client = server.https.clone();
        async move {
            let info: serde_json::Value = client
                .get(format!("{base}/System/Info/Public"))
                .send()
                .await
                .expect("public info")
                .json()
                .await
                .expect("public info body");
            info["LocalAddress"]
                .as_str()
                .expect("local address")
                .to_owned()
        }
    };
    assert_eq!(local_address(server.base.clone()).await, server.base);
    assert_eq!(
        local_address(server.https_base.clone()).await,
        server.https_base
    );

    let token = server.sign_in().await;
    server.shut_down(&token).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_certificate_path_needs_a_restart_or_is_answered_404() {
    let server = boot_https_server().await;
    let token = server.sign_in().await;
    let pending = |info: serde_json::Value| info["HasPendingRestart"].as_bool().expect("flag");
    let mut network = server
        .get_json("/System/Configuration/network", &token)
        .await;

    // A missing file is saved, and the save answered 404.
    let missing = server.root.join("missing.pfx");
    network["CertificatePath"] = serde_json::json!(missing);
    assert_eq!(
        server.save_network(&network, &token).await,
        reqwest::StatusCode::NOT_FOUND
    );
    let stored = server
        .get_json("/System/Configuration/network", &token)
        .await;
    assert_eq!(stored["CertificatePath"], serde_json::json!(missing));
    assert!(!pending(server.get_json("/System/Info", &token).await));

    // An existing file flags a restart.
    let renewed = server.root.join("renewed.pfx");
    let (renewed_der, renewed_pkcs12) = self_signed_pkcs12("secret");
    std::fs::write(&renewed, renewed_pkcs12).expect("write renewed certificate");
    network["CertificatePath"] = serde_json::json!(renewed);
    assert_eq!(
        server.save_network(&network, &token).await,
        reqwest::StatusCode::NO_CONTENT
    );
    assert!(pending(server.get_json("/System/Info", &token).await));

    // The in-process restart binds HTTPS again, with the new certificate: a
    // client that trusts only that one gets through.
    let restart = server
        .https
        .post(format!("{}/System/Restart", server.https_base))
        .header("Authorization", authorization(&token))
        .send()
        .await
        .expect("restart");
    assert_eq!(restart.status(), reqwest::StatusCode::NO_CONTENT);
    let renewed_client = reqwest::Client::builder()
        .tls_certs_only([reqwest::Certificate::from_der(&renewed_der).expect("certificate")])
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("https client");
    let deadline = std::time::Instant::now() + boot::STARTUP_DEADLINE;
    let public_info = format!("{}/System/Info/Public", server.https_base);
    while !renewed_client
        .get(&public_info)
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
    {
        assert!(
            std::time::Instant::now() < deadline,
            "HTTPS never came back with the new certificate"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    HttpsServer {
        https: renewed_client,
        ..server
    }
    .shut_down(&token)
    .await;
}
