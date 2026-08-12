//! Encoding configuration store + factory.
//!
//! Port of `MediaBrowser.MediaEncoding.Configuration`:
//! [`EncodingConfigurationStore`] (the `"encoding"`-keyed
//! [`ConfigurationStore`](ferrofin_common::configuration::ConfigurationStore) whose
//! body is [`EncodingOptions`](ferrofin_model::configuration::EncodingOptions)) and
//! [`EncodingConfigurationFactory`]. The store's
//! [`Validate`](ferrofin_common::configuration::ValidatingConfiguration::validate)
//! guard — reject an `EncoderAppPath` change and require any new
//! `TranscodingTempPath` to exist — is ported with the un-mockable directory
//! check behind the [`DirChecker`] seam so tests inject a fake.

pub mod store;

pub use store::{
    DirChecker, EncodingConfigurationFactory, EncodingConfigurationStore, RealDirChecker,
};
