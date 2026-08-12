//! Port of `MediaBrowser.Model.Notifications`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The type of a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum NotificationType {
    /// An application update is available.
    ApplicationUpdateAvailable,
    /// An application update was installed.
    ApplicationUpdateInstalled,
    /// Audio playback started.
    AudioPlayback,
    /// Video playback started.
    VideoPlayback,
    /// Audio playback stopped.
    AudioPlaybackStopped,
    /// Video playback stopped.
    VideoPlaybackStopped,
    /// An installation failed.
    InstallationFailed,
    /// A plugin errored.
    PluginError,
    /// A plugin was installed.
    PluginInstalled,
    /// A plugin update was installed.
    PluginUpdateInstalled,
    /// A plugin was uninstalled.
    PluginUninstalled,
    /// New library content is available.
    NewLibraryContent,
    /// A server restart is required.
    ServerRestartRequired,
    /// A scheduled task failed.
    TaskFailed,
    /// A user was locked out.
    UserLockedOut,
}
