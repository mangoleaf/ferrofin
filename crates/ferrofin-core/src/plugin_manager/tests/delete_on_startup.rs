//! An uninstall that cannot delete a WASM plugin's file: upstream's
//! `RemovePlugin` then marks the plugin `Deleted` and the uninstall answers
//! `204`, and `LoadAssemblies` deletes it at the next start, before anything
//! is loaded, never loading one it still cannot delete (v12.4
//! `PluginManager.cs:106-118,274-293`; `PluginStatus.Deleted`).

use super::*;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The `{id}.wasm` an install staged, as something `remove_file` cannot
/// delete whoever runs the test: a directory.
fn undeletable_staged_file(rig: &InstallRig) -> PathBuf {
    let path = rig.wasm_dir.join(format!("{PKG_ID}.wasm"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// Registers `PKG_ID` as loaded from `artifact`, as the host does at boot.
fn load_from(rig: &mut InstallRig, artifact: &Path) {
    let plugins_dir = rig.mgr.plugins_dir_for_test().to_path_buf();
    rig.mgr = FerrofinPluginManager::new(
        vec![
            RegisteredPlugin::new(descriptor(PKG_ID, "HelloPkg", true), None)
                .with_artifact(artifact.to_path_buf()),
        ],
        plugins_dir,
    )
    .with_installer(
        rig.wasm_dir.clone(),
        Arc::new(StubValidator {
            id: Ok(PKG_ID),
            abi: TEST_ABI,
        }),
        rig.lifecycle.clone(),
    );
}

/// The file names `state.json` marks for deletion at the next start.
fn marked(plugins_dir: &Path) -> Vec<String> {
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(plugins_dir.join("state.json")).unwrap()).unwrap();
    serde_json::from_value::<Vec<OsString>>(state["delete_on_startup"].clone())
        .unwrap()
        .into_iter()
        .map(|name| name.into_string().unwrap())
        .collect()
}

/// Writes a `state.json` that marks `names`, as `remove_plugin` writes them.
fn write_marks(plugins_dir: &Path, names: &[&str]) {
    let names: Vec<OsString> = names.iter().map(OsString::from).collect();
    std::fs::create_dir_all(plugins_dir).unwrap();
    std::fs::write(
        plugins_dir.join("state.json"),
        serde_json::json!({ "delete_on_startup": names }).to_string(),
    )
    .unwrap();
}

/// A directory where `state.json` goes: no rename replaces it, so no save
/// succeeds.
fn block_state_saves(plugins_dir: &Path) {
    let state = plugins_dir.join("state.json");
    std::fs::remove_file(&state).unwrap();
    std::fs::create_dir_all(state.join("taken")).unwrap();
}

/// The uninstall succeeds as one that deleted the file would: the plugin
/// leaves every lookup and the restart is flagged, and the file is marked.
#[tokio::test]
async fn an_uninstall_that_cannot_delete_the_file_marks_it_deleted() {
    let rig = update_rig("0.9.0", false, true).await;
    let file = undeletable_staged_file(&rig);

    rig.mgr
        .remove_plugin(PKG_ID)
        .await
        .expect("uninstalled, as RemovePlugin's Deleted fallback is");
    assert!(file.exists(), "nothing could delete it");
    assert_eq!(
        marked(rig.mgr.plugins_dir_for_test()),
        [format!("{PKG_ID}.wasm")]
    );
    assert!(
        rig.mgr.get_plugin(PKG_ID).await.unwrap().is_none(),
        "gone from every lookup"
    );
    assert!(rig.lifecycle.has_pending_restart());
}

/// Of a loaded file and an update staged beside it, the one deleted stays
/// deleted and only the other is marked.
#[tokio::test]
async fn only_the_file_that_cannot_be_deleted_is_marked() {
    let mut rig = update_rig("0.9.0", false, true).await;
    let staged = undeletable_staged_file(&rig);
    let loaded = rig.wasm_dir.join("myplugin.wasm");
    std::fs::write(&loaded, b"component").unwrap();
    load_from(&mut rig, &loaded);

    rig.mgr.remove_plugin(PKG_ID).await.expect("uninstall");
    assert!(!loaded.exists(), "the loaded file is deleted");
    assert!(staged.exists());
    assert_eq!(
        marked(rig.mgr.plugins_dir_for_test()),
        [format!("{PKG_ID}.wasm")]
    );
}

/// A mark `state.json` cannot take is the uninstall's error, as upstream's
/// `SaveManifest` write failure is: the plugin stays installed, and unmarked
/// in memory too, so a later save does not mark it after all.
#[tokio::test]
async fn an_uninstall_whose_mark_cannot_be_saved_answers_the_error() {
    let rig = update_rig("0.9.0", false, true).await;
    undeletable_staged_file(&rig);
    let plugins_dir = rig.mgr.plugins_dir_for_test();
    block_state_saves(plugins_dir);

    let error = rig.mgr.remove_plugin(PKG_ID).await.unwrap_err();
    assert!(matches!(error, ServiceError::Backend(_)), "{error:?}");
    assert!(
        rig.mgr.get_plugin(PKG_ID).await.unwrap().is_some(),
        "still installed"
    );
    assert!(!rig.lifecycle.has_pending_restart());

    std::fs::remove_dir_all(plugins_dir.join("state.json")).unwrap();
    rig.mgr.disable_plugin(PKG_ID).await.expect("a later save");
    assert_eq!(marked(plugins_dir), Vec::<String>::new());
}

/// A repository install, once loaded, runs from the `{id}.wasm` it staged,
/// the common case: the uninstall answers as one that deleted it, with that
/// file marked.
#[tokio::test]
async fn a_loaded_repository_install_is_marked() {
    let mut rig = update_rig("0.9.0", false, true).await;
    let file = undeletable_staged_file(&rig);
    load_from(&mut rig, &file);

    rig.mgr.remove_plugin(PKG_ID).await.expect("uninstall");
    assert_eq!(
        marked(rig.mgr.plugins_dir_for_test()),
        [format!("{PKG_ID}.wasm")]
    );
}

/// A loaded file outside the plugins directory cannot be marked by name, so
/// its failed delete is the uninstall's error, as before.
#[tokio::test]
async fn a_file_outside_the_plugins_directory_is_not_marked() {
    let mut rig = update_rig("0.9.0", false, true).await;
    let elsewhere = tempfile::tempdir().unwrap();
    let file = elsewhere.path().join("hello.wasm");
    std::fs::create_dir(&file).unwrap();
    load_from(&mut rig, &file);

    let error = rig.mgr.remove_plugin(PKG_ID).await.unwrap_err();
    assert!(matches!(error, ServiceError::Backend(_)), "{error:?}");
    assert!(rig.mgr.get_plugin(PKG_ID).await.unwrap().is_some());
}

/// A file name that is not UTF-8 is marked and deleted exactly.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_name_that_is_not_utf8_is_marked_and_deleted() {
    use std::os::unix::ffi::OsStringExt as _;
    let mut rig = update_rig("0.9.0", false, true).await;
    let name = OsString::from_vec(b"plug\xffin.wasm".to_vec());
    let file = rig.wasm_dir.join(&name);
    std::fs::create_dir_all(&file).unwrap();
    load_from(&mut rig, &file);
    rig.mgr.remove_plugin(PKG_ID).await.expect("uninstall");

    std::fs::remove_dir(&file).unwrap();
    std::fs::write(&file, b"component").unwrap();
    assert_eq!(
        FerrofinPluginManager::delete_plugins_marked_deleted(
            rig.mgr.plugins_dir_for_test(),
            &rig.wasm_dir
        ),
        Vec::<PathBuf>::new()
    );
    assert!(!file.exists(), "deleted at the start");
}

/// The start deletes a marked file and forgets it; one it still cannot
/// delete stays marked and is returned, so the host does not load it.
#[tokio::test]
async fn the_start_deletes_the_files_an_uninstall_marked_deleted() {
    let rig = update_rig("0.9.0", false, true).await;
    let file = undeletable_staged_file(&rig);
    rig.mgr.remove_plugin(PKG_ID).await.expect("uninstall");
    let plugins_dir = rig.mgr.plugins_dir_for_test();

    assert_eq!(
        FerrofinPluginManager::delete_plugins_marked_deleted(plugins_dir, &rig.wasm_dir),
        std::slice::from_ref(&file),
        "still there: not to be loaded"
    );
    assert_eq!(
        marked(plugins_dir),
        [format!("{PKG_ID}.wasm")],
        "every start tries again"
    );

    // Deletable now: deleted and forgotten. A file no uninstall marked stays.
    std::fs::remove_dir(&file).unwrap();
    std::fs::write(&file, b"component").unwrap();
    let other = rig.wasm_dir.join("other.wasm");
    std::fs::write(&other, b"component").unwrap();
    assert_eq!(
        FerrofinPluginManager::delete_plugins_marked_deleted(plugins_dir, &rig.wasm_dir),
        Vec::<PathBuf>::new()
    );
    assert!(!file.exists(), "deleted at the start");
    assert!(other.exists(), "only the marked file is touched");
    assert_eq!(marked(plugins_dir), Vec::<String>::new());
}

/// Only a `*.wasm` file name in the plugins directory is ever deleted: a mark
/// naming another place or another file is dropped, and nothing is touched.
#[test]
fn a_mark_that_is_no_plugin_file_name_deletes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    // The configuration and data directories may be one directory.
    let plugins_dir = dir.path().join("plugins");
    let outside = dir.path().join("outside.wasm");
    std::fs::write(&outside, b"keep").unwrap();
    let outside_name = outside.display().to_string();
    write_marks(
        &plugins_dir,
        &[&outside_name, "../outside.wasm", "keep.json"],
    );
    let not_a_plugin = plugins_dir.join("keep.json");
    std::fs::write(&not_a_plugin, b"{}").unwrap();

    assert_eq!(
        FerrofinPluginManager::delete_plugins_marked_deleted(&plugins_dir, &plugins_dir),
        Vec::<PathBuf>::new()
    );
    assert!(outside.exists());
    assert!(not_a_plugin.exists());
    assert_eq!(marked(&plugins_dir), Vec::<String>::new());
}

/// A missing or unreadable `state.json` marks nothing, so the start deletes
/// nothing (`new` sets an unreadable one aside).
#[test]
fn a_start_without_a_readable_state_deletes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let plugins_dir = dir.path().join("config");
    let wasm_dir = dir.path().join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::create_dir_all(&wasm_dir).unwrap();
    let file = wasm_dir.join("hello.wasm");
    std::fs::write(&file, b"component").unwrap();

    assert_eq!(
        FerrofinPluginManager::delete_plugins_marked_deleted(&plugins_dir, &wasm_dir),
        Vec::<PathBuf>::new()
    );
    std::fs::write(plugins_dir.join("state.json"), b"{\"delete_on_startup\":[").unwrap();
    assert_eq!(
        FerrofinPluginManager::delete_plugins_marked_deleted(&plugins_dir, &wasm_dir),
        Vec::<PathBuf>::new()
    );
    assert!(file.exists());
}

/// An install over a marked file makes it live again, as upstream's
/// `PopulateManifest` writes the new status over `Deleted`: the start keeps
/// the plugin installed since.
#[tokio::test]
async fn an_install_over_a_file_marked_deleted_keeps_it() {
    let artifact = b"bytes".to_vec();
    let md5 = ferrofin_common::extensions::md5_hex(&artifact);
    let rig = install_rig(&artifact, None, &md5, TEST_ABI, Ok(PKG_ID)).await;
    let file = undeletable_staged_file(&rig);
    rig.mgr.remove_plugin(PKG_ID).await.expect("uninstall");
    // The administrator makes it deletable again, then installs it again.
    std::fs::remove_dir(&file).unwrap();

    rig.mgr
        .install_package("HelloPkg", None, None, None)
        .await
        .expect("install");
    let plugins_dir = rig.mgr.plugins_dir_for_test();
    assert_eq!(marked(plugins_dir), Vec::<String>::new());
    assert_eq!(
        FerrofinPluginManager::delete_plugins_marked_deleted(plugins_dir, &rig.wasm_dir),
        Vec::<PathBuf>::new()
    );
    assert_eq!(std::fs::read(&file).unwrap(), artifact, "kept at the start");
}

/// An install whose file stays marked, because the mark cannot be forgotten,
/// is an error: the next start would delete the file.
#[tokio::test]
async fn an_install_that_cannot_forget_the_mark_answers_the_error() {
    let artifact = b"bytes".to_vec();
    let md5 = ferrofin_common::extensions::md5_hex(&artifact);
    let rig = install_rig(&artifact, None, &md5, TEST_ABI, Ok(PKG_ID)).await;
    let file = undeletable_staged_file(&rig);
    rig.mgr.remove_plugin(PKG_ID).await.expect("uninstall");
    std::fs::remove_dir(&file).unwrap();
    let plugins_dir = rig.mgr.plugins_dir_for_test();
    block_state_saves(plugins_dir);

    let error = rig
        .mgr
        .install_package("HelloPkg", None, None, None)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ServiceError::Backend(why) if why.contains("state.json")),
        "{error:?}"
    );
    assert!(file.exists(), "written, and deleted at the next start");
    assert_eq!(rig.mgr.completed_installations().len(), 0);
}
