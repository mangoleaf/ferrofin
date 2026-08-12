//! Port of `MediaBrowser.Model.Dlna.ProfileCondition`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::enums::{ProfileConditionType, ProfileConditionValue};

/// A single condition a stream property must satisfy for a profile to apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct ProfileCondition {
    /// The comparison to apply.
    pub condition: ProfileConditionType,
    /// The stream property the condition constrains.
    pub property: ProfileConditionValue,
    /// The value compared against.
    pub value: String,
    /// Whether the condition is required (as opposed to a preference).
    pub is_required: bool,
}

impl ProfileCondition {
    /// Creates a condition, defaulting `is_required` to `false`.
    ///
    /// Mirrors the three-argument C# constructor.
    #[must_use]
    pub fn new(
        condition: ProfileConditionType,
        property: ProfileConditionValue,
        value: String,
    ) -> Self {
        Self::with_required(condition, property, value, false)
    }

    /// Creates a condition with an explicit `is_required` flag.
    ///
    /// Mirrors the four-argument C# constructor.
    #[must_use]
    pub fn with_required(
        condition: ProfileConditionType,
        property: ProfileConditionValue,
        value: String,
        is_required: bool,
    ) -> Self {
        Self {
            condition,
            property,
            value,
            is_required,
        }
    }
}

impl Default for ProfileCondition {
    /// The parameterless C# constructor sets `IsRequired = true`.
    fn default() -> Self {
        Self {
            condition: ProfileConditionType::Equals,
            property: ProfileConditionValue::AudioChannels,
            value: String::new(),
            is_required: true,
        }
    }
}
