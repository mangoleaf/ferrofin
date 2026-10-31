//! Audiobook parsing — port of the `Emby.Naming.AudioBook` namespace.

mod audio_book_file_info;
mod audio_book_file_path_parser;
mod audio_book_file_path_parser_result;
mod audio_book_info;
mod audio_book_list_resolver;
mod audio_book_name_parser;
mod audio_book_name_parser_result;
mod audio_book_resolver;

pub use audio_book_file_info::AudioBookFileInfo;
pub use audio_book_file_path_parser::AudioBookFilePathParser;
pub use audio_book_file_path_parser_result::AudioBookFilePathParserResult;
pub use audio_book_info::AudioBookInfo;
pub use audio_book_list_resolver::AudioBookListResolver;
pub use audio_book_name_parser::AudioBookNameParser;
pub use audio_book_name_parser_result::AudioBookNameParserResult;
pub use audio_book_resolver::AudioBookResolver;
