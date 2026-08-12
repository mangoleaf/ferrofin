//! Port of `MediaBrowser.Model.Dlna.ContainerProfile`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::enums::DlnaProfileType;
use super::profile_condition::ProfileCondition;
use crate::extensions::contains_container;

/// Optional conditions a container must meet; failing them forces transcoding.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[serde(default)]
pub struct ContainerProfile {
    /// The DLNA profile type this container must meet.
    #[serde(rename = "Type")]
    pub profile_type: DlnaProfileType,
    /// The conditions applied to the container.
    pub conditions: Vec<ProfileCondition>,
    /// The container(s) this profile must meet, comma-delimited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// The sub-container(s) this profile must meet, comma-delimited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_container: Option<String>,
}

impl ContainerProfile {
    /// Returns `true` if an item in `container` appears in [`Self::container`]
    /// (or [`Self::sub_container`] when `use_sub_container` is set and the
    /// container is `hls`).
    #[must_use]
    pub fn contains_container(&self, container: Option<&str>, use_sub_container: bool) -> bool {
        let container_to_check = if use_sub_container
            && self
                .container
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case("hls"))
        {
            self.sub_container.as_deref()
        } else {
            self.container.as_deref()
        };
        contains_container(container_to_check, container)
    }
}
