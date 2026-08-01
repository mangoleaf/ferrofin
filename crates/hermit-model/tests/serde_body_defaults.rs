//! Regression guard for the jellyfin-web deserialization class (the `TypeOptions`
//! missing-`ImageOptions` 422): every JSON request-body DTO whose fields are all
//! optional-by-default must deserialize from `{}`, so a client omitting a field it
//! doesn't know about fills from default (matching Jellyfin's System.Text.Json)
//! rather than being rejected with a 422 at the axum Json extractor.
//!
//! Audited surface = types used as `Json<T>` request-body extractors in hermit-api
//! that carry required (non-Option) fields. If someone drops the container
//! `#[serde(default)]` from one of these, this test fails.

use hermit_model::branding::BrandingOptionsDto;
use hermit_model::configuration::{ServerConfiguration, TypeOptions, UserConfiguration};
use hermit_model::dto::{ClientCapabilitiesDto, DeviceOptionsDto, UserDto};
use hermit_model::live_tv::{
    ListingsProviderInfo, SeriesTimerInfoDto, TimerInfoDto, TunerHostInfo,
};
use hermit_model::users::UserPolicy;

/// Deserialize `{}` into `$t` and assert it succeeds — the missing-field-fills-default invariant.
macro_rules! assert_empty_ok {
    ($($t:ty),+ $(,)?) => {$(
        let r: Result<$t, _> = serde_json::from_str("{}");
        assert!(r.is_ok(), concat!(stringify!($t), " must deserialize from `{{}}` (needs container #[serde(default)])"));
    )+};
}

#[test]
fn json_body_dtos_deserialize_from_empty_object() {
    assert_empty_ok!(
        TypeOptions,
        BrandingOptionsDto,
        ClientCapabilitiesDto,
        DeviceOptionsDto,
        ListingsProviderInfo,
        SeriesTimerInfoDto,
        TimerInfoDto,
        TunerHostInfo,
        ServerConfiguration,
        UserConfiguration,
        UserDto,
        UserPolicy,
    );
}
