//! The Intro Skipper plugin's per-season analyzer settings (port of
//! `IntroSkipper.Data.AnalysisMode` / `AnalyzerAction`, intro-skipper `db09359`).
//!
//! Both bind through Jellyfin's `JsonStringEnumConverter` rules
//! ([`crate::json::enums`]): a name in any case or the underlying number, an
//! undefined number kept as `Unrecognized` (C# keeps any `int`). They serialize
//! as the C# converter writes them — the name, or the bare number when no
//! member names it. Discriminants are copied from the plugin's sources; these
//! enums are absent from Jellyfin's core assembly reflection inventory.

use serde::{Deserialize, Serialize, Serializer};

/// Implements the converter traits for an analyzer enum.
macro_rules! analyzer_enum {
    ($name:ident { $($variant:ident = $value:literal),* $(,)? }) => {
        impl $name {
            /// The C# underlying integer, including values without a declared name.
            #[must_use]
            pub const fn value(self) -> i32 {
                match self {
                    $(Self::$variant => $value,)*
                    Self::Unrecognized(value) => value,
                }
            }
        }

        impl crate::json::enums::JsonEnum for $name {
            fn from_discriminant(value: i32) -> Self {
                match value { $($value => Self::$variant,)* other => Self::Unrecognized(other) }
            }
            fn members() -> &'static [(&'static str, i32)] {
                &[$((stringify!($variant), $value)),*]
            }
            fn json_default() -> Option<i32> {
                None
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                crate::json::enums::deserialize(d)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                match self {
                    $(Self::$variant => serializer.serialize_str(stringify!($variant)),)*
                    Self::Unrecognized(value) => serializer.serialize_i32(*value),
                }
            }
        }
    };
}

/// The segment kind an analysis looks for. Port of `AnalysisMode`. `Ord` is
/// the C# declaration order (`Enum.GetValues`), named modes first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AnalysisMode {
    /// The opening titles.
    Introduction,
    /// The end credits.
    Credits,
    /// The next-episode preview.
    Preview,
    /// The "previously on" recap.
    Recap,
    /// A commercial break.
    Commercial,
    /// An unnamed underlying integer, kept as C# keeps it.
    Unrecognized(i32),
}

analyzer_enum!(AnalysisMode { Introduction = 0, Credits = 1, Preview = 2, Recap = 3, Commercial = 4 });

impl AnalysisMode {
    /// Every declared mode, in the C# declaration order (`Enum.GetValues`).
    pub const ALL: [Self; 5] = [
        Self::Introduction,
        Self::Credits,
        Self::Preview,
        Self::Recap,
        Self::Commercial,
    ];
}

/// How a season's analysis of one mode runs. Port of `AnalyzerAction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum AnalyzerAction {
    /// The plugin's configured analyzer chain.
    #[default]
    Default,
    /// Chapter names first.
    Chapter,
    /// Audio fingerprints first.
    Chromaprint,
    /// Black frames first.
    BlackFrame,
    /// No analysis of this mode for the season.
    None,
    /// An unnamed underlying integer, kept as C# keeps it.
    Unrecognized(i32),
}

analyzer_enum!(AnalyzerAction { Default = 0, Chapter = 1, Chromaprint = 2, BlackFrame = 3, None = 4 });

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::enums::JsonEnum as _;

    #[test]
    fn discriminants_round_trip_and_match_the_plugin() {
        for mode in AnalysisMode::ALL {
            assert_eq!(AnalysisMode::from_discriminant(mode.value()), mode);
        }
        assert_eq!(AnalysisMode::Recap.value(), 3);
        assert_eq!(AnalyzerAction::from_discriminant(4), AnalyzerAction::None);
        assert_eq!(
            AnalyzerAction::from_discriminant(9),
            AnalyzerAction::Unrecognized(9)
        );
    }

    #[test]
    fn they_serialize_as_the_csharp_converter_writes_them() {
        assert_eq!(
            serde_json::to_string(&AnalyzerAction::BlackFrame).unwrap(),
            "\"BlackFrame\""
        );
        assert_eq!(
            serde_json::to_string(&AnalyzerAction::Unrecognized(7)).unwrap(),
            "7"
        );
        let map: std::collections::HashMap<_, _> =
            [(AnalysisMode::Credits, AnalyzerAction::None)].into();
        assert_eq!(
            serde_json::to_string(&map).unwrap(),
            r#"{"Credits":"None"}"#
        );
    }
}
