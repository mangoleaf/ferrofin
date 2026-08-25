//! A minimal port of .NET's `System.Version` — just enough for ffmpeg version
//! parsing and comparison against the recommended minimum/library versions.
//!
//! C# `System.Version` stores four components (major, minor, build, revision)
//! where any *unspecified* trailing component is `-1`, and ordering compares
//! those four signed integers lexicographically. This type reproduces that
//! behaviour so the ported [`super::encoder_validator`] comparisons match the
//! C# oracle byte-for-byte.

use std::cmp::Ordering;
use std::fmt;

/// A version with the same comparison semantics as .NET `System.Version`.
///
/// Unspecified `build`/`revision` components are represented as `-1`, exactly
/// as `System.Version` does, so `Version(4, 4)` (build `-1`) sorts *before*
/// `Version(4, 4, 0)` (build `0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FfmpegVersion {
    major: i32,
    minor: i32,
    build: i32,
    revision: i32,
}

impl FfmpegVersion {
    /// Creates a two-component version (`build`/`revision` unspecified, i.e. `-1`).
    #[must_use]
    pub const fn new(major: i32, minor: i32) -> Self {
        Self {
            major,
            minor,
            build: -1,
            revision: -1,
        }
    }

    /// Creates a three-component version (`revision` unspecified, i.e. `-1`).
    #[must_use]
    pub const fn with_build(major: i32, minor: i32, build: i32) -> Self {
        Self {
            major,
            minor,
            build,
            revision: -1,
        }
    }

    /// Creates a fully specified four-component version.
    ///
    /// This is the shape .NET's `Environment.OSVersion.Version` always has on
    /// Unix — it fills every component, using `0` for the ones the release
    /// string did not supply — which is why it sorts *after* a
    /// [`with_build`](Self::with_build) version of the same three leading
    /// numbers.
    #[must_use]
    pub const fn with_revision(major: i32, minor: i32, build: i32, revision: i32) -> Self {
        Self {
            major,
            minor,
            build,
            revision,
        }
    }

    /// Parses a dotted version string the way .NET `Version.TryParse` does.
    ///
    /// Accepts 2–4 numeric components separated by `.`; each must be a
    /// non-negative integer. Returns `None` when the shape is invalid (fewer
    /// than two components, an empty/out-of-range component, or non-numeric).
    #[must_use]
    pub fn try_parse(input: &str) -> Option<Self> {
        let parts: Vec<&str> = input.split('.').collect();
        if parts.len() < 2 || parts.len() > 4 {
            return None;
        }

        let mut nums = [-1_i32; 4];
        for (slot, part) in nums.iter_mut().zip(parts.iter()) {
            let value: i32 = part.parse().ok()?;
            if value < 0 {
                return None;
            }
            *slot = value;
        }

        Some(Self {
            major: nums[0],
            minor: nums[1],
            build: nums[2],
            revision: nums[3],
        })
    }
}

impl PartialOrd for FfmpegVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FfmpegVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.build, self.revision).cmp(&(
            other.major,
            other.minor,
            other.build,
            other.revision,
        ))
    }
}

impl fmt::Display for FfmpegVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `System.Version.ToString()` only emits the specified components.
        write!(f, "{}.{}", self.major, self.minor)?;
        if self.build >= 0 {
            write!(f, ".{}", self.build)?;
        }
        if self.revision >= 0 {
            write!(f, ".{}", self.revision)?;
        }
        Ok(())
    }
}
