//! Application-paths abstraction ported from
//! `MediaBrowser.Common.Configuration.IApplicationPaths`.
//!
//! The interface exposes the well-known directories a Jellyfin host uses, plus
//! two provisioning helpers. No concrete implementation ships in this crate —
//! the host wires one up; here we define the contract.

use std::path::{Path, PathBuf};

/// The well-known application paths and their provisioning helpers.
///
/// Port of `IApplicationPaths`. Path getters return owned `PathBuf`s (C# string
/// path getters); the two `fs`-touching methods return `io::Result` in place of
/// C# `void` + thrown exceptions.
pub trait ApplicationPaths {
    /// The path to the program data folder.
    fn program_data_path(&self) -> PathBuf;

    /// The path to the web UI resources folder.
    ///
    /// Not relevant if the server hosts no static web content.
    fn web_path(&self) -> PathBuf;

    /// The path to the program system folder.
    fn program_system_path(&self) -> PathBuf;

    /// The folder path to the data directory.
    fn data_path(&self) -> PathBuf;

    /// The image cache path.
    fn image_cache_path(&self) -> PathBuf;

    /// The path to the plugin directory.
    fn plugins_path(&self) -> PathBuf;

    /// The path to the plugin configurations directory.
    fn plugin_configurations_path(&self) -> PathBuf;

    /// The path to the log directory.
    fn log_directory_path(&self) -> PathBuf;

    /// The path to the application configuration root directory.
    fn configuration_directory_path(&self) -> PathBuf;

    /// The path to the system configuration file.
    fn system_configuration_file_path(&self) -> PathBuf;

    /// The folder path to the cache directory.
    fn cache_path(&self) -> PathBuf;

    /// The folder path to the temp directory within the cache folder.
    fn temp_directory(&self) -> PathBuf;

    /// The magic string used for virtual path manipulation.
    fn virtual_data_path(&self) -> PathBuf;

    /// The path used for storing trickplay files.
    fn trickplay_path(&self) -> PathBuf;

    /// The path used for storing backup archives.
    fn backup_path(&self) -> PathBuf;

    /// Checks and creates all known base paths.
    ///
    /// # Errors
    ///
    /// Returns an error if any base path cannot be created or validated.
    fn make_sanity_check_or_throw(&self) -> std::io::Result<()>;

    /// Checks and creates `path`, adding a marker file if it does not exist.
    ///
    /// `recursive` checks for other settings paths recursively.
    ///
    /// # Errors
    ///
    /// Returns an error if the path or marker file cannot be created.
    fn create_and_check_marker(
        &self,
        path: &Path,
        marker_name: &str,
        recursive: bool,
    ) -> std::io::Result<()>;
}
