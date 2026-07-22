//! Leaf utilities for Hermit — the Rust port of Jellyfin's `Jellyfin.Extensions`.
//!
//! Each module mirrors a C# file from the `Jellyfin.Extensions` namespace,
//! translated to idiomatic Rust while preserving observable behavior (the
//! upstream xUnit tests are ported verbatim as the oracle). See
//! `brain/PLAN_HERMIT_PORT.md`.

pub mod copy_to_extensions;
pub mod dictionary_extensions;
pub mod enumerable_extensions;
pub mod error;
pub mod file_helper;
pub mod formatting_stream_writer;
pub mod guid_extensions;
pub mod path_helper;
pub mod read_only_list_extension;
pub mod shuffle_extensions;
pub mod split_string_extensions;
pub mod stream_extensions;
pub mod string_builder_extensions;
pub mod string_extensions;
