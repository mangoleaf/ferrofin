//! Video parsing — port of the `Emby.Naming.Video` namespace.

pub mod clean_date_time_parser;
mod clean_date_time_result;
pub mod clean_string_parser;
mod extra_result;
mod extra_rule;
pub mod extra_rule_resolver;
mod extra_rule_type;
mod file_stack;
mod file_stack_rule;
pub mod format_3d_parser;
mod format_3d_result;
mod format_3d_rule;
mod numeric_ordering;
pub mod stack_resolver;
pub mod stub_resolver;
mod stub_type_rule;
mod video_file_info;
mod video_info;
mod video_list_resolver;
pub mod video_resolver;

pub use clean_date_time_result::CleanDateTimeResult;
pub use extra_result::ExtraResult;
pub use extra_rule::ExtraRule;
pub use extra_rule_type::ExtraRuleType;
pub use file_stack::FileStack;
pub use file_stack_rule::{FileStackMatch, FileStackRule};
pub use format_3d_result::Format3DResult;
pub use format_3d_rule::Format3DRule;
pub use stub_resolver::StubResult;
pub use stub_type_rule::StubTypeRule;
pub use video_file_info::VideoFileInfo;
pub use video_info::VideoInfo;
pub use video_list_resolver::VideoListResolver;
pub use video_resolver::{is_stub_file, is_video_file};
