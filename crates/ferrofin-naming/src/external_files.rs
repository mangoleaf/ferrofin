//! External file parsing — port of the `Emby.Naming.ExternalFiles` namespace.

mod external_path_parser;
mod external_path_parser_result;
mod localization;

pub use external_path_parser::ExternalPathParser;
pub use external_path_parser_result::ExternalPathParserResult;
pub use localization::LocalizationManager;
