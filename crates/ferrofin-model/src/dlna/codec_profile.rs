//! Port of `MediaBrowser.Model.Dlna.CodecProfile`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::enums::CodecType;
use super::profile_condition::ProfileCondition;
use crate::extensions::{contains_container, contains_container_with_negation};

/// Conditions a codec must meet, and further conditions applied once it does.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[serde(default)]
pub struct CodecProfile {
    /// The codec type this profile must meet.
    #[serde(rename = "Type")]
    pub codec_type: CodecType,
    /// The conditions this profile must meet.
    pub conditions: Vec<ProfileCondition>,
    /// The conditions to apply once this profile is met.
    pub apply_conditions: Vec<ProfileCondition>,
    /// The codec(s) this profile applies to, comma-delimited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// The container(s) this profile applies to, comma-delimited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// The sub-container(s) this profile applies to, comma-delimited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_container: Option<String>,
}

impl CodecProfile {
    /// Returns `true` if any of `codecs` and the given `container` are covered
    /// by this profile.
    #[must_use]
    pub fn contains_any_codec(
        &self,
        codecs: &[&str],
        container: Option<&str>,
        use_sub_container: bool,
    ) -> bool {
        let container_to_check = self.container_to_check(use_sub_container);
        contains_container(container_to_check, container)
            && codecs
                .iter()
                .any(|c| contains_container_with_negation(self.codec.as_deref(), false, Some(c)))
    }

    /// Single-codec convenience over [`Self::contains_any_codec`].
    #[must_use]
    pub fn contains_codec(
        &self,
        codec: Option<&str>,
        container: Option<&str>,
        use_sub_container: bool,
    ) -> bool {
        let container_to_check = self.container_to_check(use_sub_container);
        contains_container(container_to_check, container)
            && contains_container_with_negation(self.codec.as_deref(), false, codec)
    }

    /// Resolves the container to match against, honouring the `hls`
    /// sub-container fallback.
    fn container_to_check(&self, use_sub_container: bool) -> Option<&str> {
        if use_sub_container
            && self
                .container
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case("hls"))
        {
            self.sub_container.as_deref()
        } else {
            self.container.as_deref()
        }
    }
}
