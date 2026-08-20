//! Shared naming primitives — port of the `Emby.Naming.Common` namespace.

mod episode_expression;
mod guarded_regex;
mod media_type;
mod naming_options;

pub use episode_expression::EpisodeExpression;
pub use guarded_regex::GuardedRegex;
pub use media_type::MediaType;
pub use naming_options::NamingOptions;
