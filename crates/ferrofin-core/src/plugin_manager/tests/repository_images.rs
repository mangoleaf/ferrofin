//! A repository plugin's image: `PluginManager.PopulateManifest`'s
//! `ImageUrl` download at install and on every catalog fetch (v12.4
//! `PluginManager.cs:388-485`, `InstallationManager.cs:193-198,619`), and
//! the stored file served once the plugin is loaded (`HasImage`,
//! `GetPluginImage`).

use super::*;
use std::sync::atomic::AtomicUsize;

/// The first bytes of a PNG, which is all the image check reads.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nicon-bytes";

/// A repository at `/manifest.json` listing `HelloPkg` 1.0.0 with
/// `imageUrl` `{base}{image_path}`, its artifact at `/plugin.wasm`, and
/// `image` answered at `image_path`, counting the requests for it.
fn image_repo(
    image_path: &'static str,
    image: Vec<u8>,
) -> (String, std::sync::mpsc::Sender<()>, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&hits);
    let (base, stop) = raw_server(move |base| {
        let md5 = ferrofin_common::extensions::md5_hex(b"bytes");
        let manifest = format!(
            r#"[{{"name":"HelloPkg","description":"d","overview":"o","owner":"me","category":"General",
                "guid":"{PKG_ID}","imageUrl":"{base}{image_path}",
                "versions":[{{"version":"1.0.0","targetAbi":"{TEST_ABI}","sourceUrl":"{base}/plugin.wasm","checksum":"{md5}"}}]}}]"#
        );
        Box::new(move |request: &str| {
            if request.contains("/plugin.wasm") {
                http_ok("application/wasm", b"bytes")
            } else if request.contains(image_path) {
                counted.fetch_add(1, Ordering::SeqCst);
                image.clone()
            } else {
                http_ok("application/json", manifest.as_bytes())
            }
        })
    });
    (base, stop, hits)
}

/// A manager reading `base`'s repository, with `plugins` registered as the
/// boot loaded them and the installer armed on `wasm_dir`.
async fn manager_at(
    state_dir: &std::path::Path,
    wasm_dir: &std::path::Path,
    base: &str,
    plugins: Vec<RegisteredPlugin>,
) -> FerrofinPluginManager {
    let mgr = FerrofinPluginManager::new(plugins, state_dir.to_path_buf()).with_installer(
        wasm_dir.to_path_buf(),
        Arc::new(StubValidator {
            id: Ok(PKG_ID),
            abi: TEST_ABI,
        }),
        Arc::new(FlagLifecycle(AtomicBool::new(false))),
    );
    mgr.set_repositories(vec![RepositoryInfo {
        name: Some("test".to_owned()),
        url: Some(format!("{base}/manifest.json")),
        enabled: true,
    }])
    .await
    .unwrap();
    mgr
}

/// `HelloPkg` at `version`, loaded from `artifact` (a runtime plugin).
fn loaded(version: &str, artifact: &std::path::Path) -> RegisteredPlugin {
    let mut d = descriptor(PKG_ID, "HelloPkg", true);
    d.version = version.to_owned();
    RegisteredPlugin::new(d, None).with_artifact(artifact.to_path_buf())
}

/// The install fetches the image into the plugin's own directory in place of
/// the one an earlier version left (upstream extracts each version into a
/// fresh folder), the boot that loads the plugin serves it typed by its
/// extension, and the uninstall takes it away.
#[tokio::test]
async fn an_install_stores_the_image_the_boot_serves() {
    ferrofin_providers::destination::test_support::allow_loopback_images();
    let (state, wasm) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (base, _stop, hits) = image_repo("/icon.png", http_ok("image/png", PNG));
    let plugin_dir = state.path().join(PKG_ID.to_string());
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(plugin_dir.join("image.jpg"), b"\xFF\xD8\xFFold").unwrap();

    let mgr = manager_at(state.path(), wasm.path(), &base, Vec::new()).await;
    mgr.install_package("HelloPkg", None, None, None)
        .await
        .expect("install");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read(plugin_dir.join("image.png")).unwrap(), PNG);
    assert!(
        !plugin_dir.join("image.jpg").exists(),
        "the earlier image went"
    );

    let artifact = wasm.path().join(format!("{PKG_ID}.wasm"));
    let booted = manager_at(
        state.path(),
        wasm.path(),
        &base,
        vec![loaded("1.0.0", &artifact)],
    )
    .await;
    assert!(booted.get_plugin(PKG_ID).await.unwrap().unwrap().has_image);
    assert_eq!(
        booted.plugin_image(PKG_ID).await.unwrap(),
        Some(PluginImage {
            content_type: "image/png".to_owned(),
            source: PluginImageSource::File(plugin_dir.join("image.png")),
        })
    );

    booted.remove_plugin(PKG_ID).await.expect("uninstall");
    assert!(!plugin_dir.exists(), "the image goes with the plugin");
}

/// A catalog fetch downloads the image of an installed runtime plugin whose
/// id and version the repository lists, only while it has none
/// (`ImageExists`: a file that is there and not empty), and never for a
/// version it does not list or a compiled-in plugin of that id.
#[tokio::test]
async fn a_catalog_fetch_downloads_only_a_missing_image() {
    ferrofin_providers::destination::test_support::allow_loopback_images();
    let (state, wasm) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (base, _stop, hits) = image_repo("/art/icon.png", http_ok("image/png", PNG));
    let artifact = wasm.path().join("hand-dropped.wasm");
    let image = state.path().join(PKG_ID.to_string()).join("image.png");

    for plugin in [
        loaded("0.9.0", &artifact),
        RegisteredPlugin::new(descriptor(PKG_ID, "HelloPkg", true), None),
    ] {
        let mgr = manager_at(state.path(), wasm.path(), &base, vec![plugin]).await;
        mgr.list_packages().await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    let mgr = manager_at(
        state.path(),
        wasm.path(),
        &base,
        vec![loaded("1.0.0", &artifact)],
    )
    .await;
    mgr.list_packages().await.unwrap();
    assert_eq!(std::fs::read(&image).unwrap(), PNG);
    mgr.list_packages().await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 1, "a present image is kept");

    // An interrupted download may leave an empty file behind.
    std::fs::write(&image, b"").unwrap();
    mgr.list_packages().await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_eq!(std::fs::read(&image).unwrap(), PNG);
}

/// An image that cannot be had costs only the image (`DownloadImage` returns
/// no path): the install succeeds, nothing is stored, and the loaded plugin
/// reports none. A body that is not an image is refused like any artwork's.
#[rstest::rstest]
#[case::missing(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec())]
#[case::not_an_image(http_ok("image/png", b"{\"secret\":true}"))]
#[tokio::test]
async fn an_image_that_fails_leaves_the_plugin_without_one(#[case] answer: Vec<u8>) {
    ferrofin_providers::destination::test_support::allow_loopback_images();
    let (state, wasm) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (base, _stop, hits) = image_repo("/icon.png", answer);

    let mgr = manager_at(state.path(), wasm.path(), &base, Vec::new()).await;
    mgr.install_package("HelloPkg", None, None, None)
        .await
        .expect("the install does not depend on its image");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert!(!state.path().join(PKG_ID.to_string()).exists());

    let artifact = wasm.path().join(format!("{PKG_ID}.wasm"));
    let booted = manager_at(
        state.path(),
        wasm.path(),
        &base,
        vec![loaded("1.0.0", &artifact)],
    )
    .await;
    assert!(!booted.get_plugin(PKG_ID).await.unwrap().unwrap().has_image);
    assert_eq!(booted.plugin_image(PKG_ID).await.unwrap(), None);
}

/// The URL picks only the stored file's extension, letters and digits naming
/// an image type, so it can name neither another of the plugin's files, nor a
/// path outside its directory, nor a type the image route must not answer
/// with.
#[rstest::rstest]
#[case("https://repo.example/images/plugin.png", "image.png")]
#[case("https://repo.example/plugin.SVG?v=2", "image.SVG")]
#[case("https://repo.example/logo.jpeg", "image.jpeg")]
#[case("https://repo.example/config.json", "image")]
#[case("https://repo.example/icon.html", "image")]
#[case("https://repo.example/icon", "image")]
#[case("https://repo.example/images/", "image")]
#[case("https://repo.example/a.b%2F..%2Fx", "image")]
#[case("https://repo.example/..%2F..%2Fstate.json", "image")]
fn the_url_names_only_the_extension(#[case] url: &str, #[case] expected: &str) {
    assert_eq!(super::super::image_file_name(url), expected);
}

/// `GetPlugin(id, version)`'s `Version.Equals`: a missing component is not a
/// zero, padding is not a difference, and a string no `Version` parses
/// matches only itself.
#[rstest::rstest]
#[case("1.0.0", "1.0.0", true)]
#[case("01.0.0", "1.0.0", true)]
#[case("1.0", "1.0.0", false)]
#[case("1.0.0", "1.0.1", false)]
#[case("1.0.0-rc1", "1.0.0-rc1", true)]
#[case("1.0.0-rc1", "1.0.0", false)]
fn versions_match_as_dotnet_versions(#[case] a: &str, #[case] b: &str, #[case] same: bool) {
    assert_eq!(super::super::same_version(a, b), same);
}
