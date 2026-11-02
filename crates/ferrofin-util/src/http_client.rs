//! Defaults of .NET's `System.Net.Http.HttpClient` that Jellyfin's outbound
//! requests inherit, for the Ferrofin clients that port those requests.

use std::time::Duration;

/// .NET's default `HttpClient.Timeout`: 100 seconds.
///
/// Jellyfin's `NamedClient.Default` (`Jellyfin.Server/Startup.cs`) never sets
/// it, and neither does the File Transformation plugin's `new HttpClient()`
/// (`TransformationHelper.ApplyTransformation`), so the requests they send give
/// up after 100 s. A request sent with `HttpCompletionOption.ResponseHeadersRead`
/// is bounded up to its response headers and its body is read without a limit;
/// any other request is bounded in full, body included.
pub const HTTP_CLIENT_DEFAULT_TIMEOUT: Duration = Duration::from_secs(100);
