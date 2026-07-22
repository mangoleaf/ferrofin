//! Audio parsing helpers — port of the `Emby.Naming.Audio` namespace.

mod album_parser;
mod audio_file_parser;

pub use album_parser::AlbumParser;
pub use audio_file_parser::is_audio_file;
