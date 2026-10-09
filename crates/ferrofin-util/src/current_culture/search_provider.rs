//! Portable search collation payloads generated from the official ICU 78.1rc export.
//! SPDX-License-Identifier: Unicode-3.0
use std::cmp::Ordering;
use std::sync::OnceLock;

use icu_collator::provider::{
    CollationData, CollationDiacriticsV1, CollationJamoV1, CollationMetadata, CollationMetadataV1,
    CollationReorderingV1, CollationRootV1, CollationSpecialPrimariesV1, CollationTailoringV1,
};
use icu_collator::{Collator, CollatorPreferences, options::CollatorOptions};
use icu_collections::codepointtrie::{CodePointTrie, CodePointTrieHeader};
use icu_locale_core::{Locale, extensions::unicode::key};
use icu_locale_fallback::LocaleFallbacker;
use icu_normalizer::provider::{NormalizerNfdDataV1, NormalizerNfdTablesV1};
use icu_provider::prelude::*;
use zerovec::ZeroVec;

#[path = "generated_search_data.rs"]
mod generated_search_data;
#[path = "search_hangul.rs"]
mod search_hangul;

#[derive(Clone, Copy)]
struct RawSearchData {
    locale: &'static str,
    attribute: &'static str,
    bits: u32,
    header: CodePointTrieHeader,
    index: &'static [u16],
    data: &'static [u32],
    ces: &'static [u64],
    ce32s: &'static [u32],
    contexts: &'static [u16],
}

impl RawSearchData {
    fn data(&self) -> Result<CollationData<'static>, DataError> {
        let trie = CodePointTrie::try_new(
            self.header,
            ZeroVec::alloc_from_slice(self.index),
            ZeroVec::alloc_from_slice(self.data),
        )
        .map_err(|error| {
            DataError::custom("invalid official search trie").with_display_context(&error)
        })?;
        Ok(CollationData {
            trie,
            ces: ZeroVec::alloc_from_slice(self.ces),
            ce32s: ZeroVec::alloc_from_slice(self.ce32s),
            contexts: ZeroVec::alloc_from_slice(self.contexts),
        })
    }
}

struct SearchEntry {
    locale: DataLocale,
    attribute: &'static str,
    metadata: CollationMetadata,
    data: CollationData<'static>,
}

fn entries() -> Result<&'static [SearchEntry], DataError> {
    static ENTRIES: OnceLock<Result<Vec<SearchEntry>, DataError>> = OnceLock::new();
    ENTRIES
        .get_or_init(|| {
            generated_search_data::SOURCES
                .iter()
                .map(|raw| {
                    Ok(SearchEntry {
                        locale: DataLocale::try_from_str(raw.locale).map_err(|error| {
                            DataError::custom("invalid source locale").with_display_context(&error)
                        })?,
                        attribute: raw.attribute,
                        metadata: CollationMetadata { bits: raw.bits },
                        data: raw.data()?,
                    })
                })
                .collect()
        })
        .as_ref()
        .map(Vec::as_slice)
        .map_err(|error| *error)
}

fn find_entry(
    req: DataRequest<'_>,
) -> Result<(&'static SearchEntry, Option<DataLocale>), DataError> {
    match find_exact_entry(req) {
        Ok(entry) => Ok(entry),
        Err(error)
            if req.id.marker_attributes.as_str().starts_with("search")
                && req.id.marker_attributes.as_str().len() > 6 =>
        {
            // ICU 78 CollationLoader::loadFromCollations retries search before default/standard.
            let attribute = DataMarkerAttributes::from_str_or_panic("search");
            let fallback_req = DataRequest {
                id: DataIdentifierBorrowed::for_marker_attributes_and_locale(
                    attribute,
                    req.id.locale,
                ),
                ..req
            };
            find_exact_entry(fallback_req).map_err(|_| error)
        }
        Err(error) => Err(error),
    }
}

fn find_exact_entry(
    req: DataRequest<'_>,
) -> Result<(&'static SearchEntry, Option<DataLocale>), DataError> {
    let entries = entries()?;
    let attribute = req.id.marker_attributes.as_str();
    if let Some(entry) = entries
        .iter()
        .find(|entry| entry.locale == *req.id.locale && entry.attribute == attribute)
    {
        return Ok((entry, None));
    }
    // This is the same marker-specific ICU4X fallback used by the installed Baked provider.
    let fallbacker = LocaleFallbacker::new().for_config(CollationTailoringV1::INFO.fallback_config);
    let mut fallback = fallbacker.fallback_for(*req.id.locale);
    loop {
        if let Some(entry) = entries
            .iter()
            .find(|entry| &entry.locale == fallback.get() && entry.attribute == attribute)
        {
            return Ok((entry, Some(fallback.take())));
        }
        if fallback.get().is_unknown() {
            return Err(DataErrorKind::IdentifierNotFound.with_req(CollationTailoringV1::INFO, req));
        }
        fallback.step();
    }
}

// Search payloads share only the exact compatible compiled root/normalizer data.
struct SearchDataProvider;

impl DataProvider<CollationTailoringV1> for SearchDataProvider {
    fn load(&self, req: DataRequest<'_>) -> Result<DataResponse<CollationTailoringV1>, DataError> {
        if !req.id.marker_attributes.as_str().starts_with("search") {
            return DataProvider::<CollationTailoringV1>::load(&icu_collator::provider::Baked, req);
        }
        let (entry, locale) = find_entry(req)?;
        let mut metadata = DataResponseMetadata::default();
        metadata.locale = locale;
        Ok(DataResponse {
            payload: DataPayload::from_static_ref(&entry.data),
            metadata,
        })
    }
}

impl DataProvider<CollationMetadataV1> for SearchDataProvider {
    fn load(&self, req: DataRequest<'_>) -> Result<DataResponse<CollationMetadataV1>, DataError> {
        if !req.id.marker_attributes.as_str().starts_with("search") {
            return DataProvider::<CollationMetadataV1>::load(&icu_collator::provider::Baked, req);
        }
        let (entry, locale) = find_entry(req)?;
        let mut metadata = DataResponseMetadata::default();
        metadata.locale = locale;
        Ok(DataResponse {
            payload: DataPayload::from_static_ref(&entry.metadata),
            metadata,
        })
    }
}

macro_rules! forward {
    ($marker:ty, $provider:path) => {
        impl DataProvider<$marker> for SearchDataProvider {
            fn load(&self, req: DataRequest<'_>) -> Result<DataResponse<$marker>, DataError> {
                DataProvider::<$marker>::load(&$provider, req)
            }
        }
    };
}
forward!(CollationRootV1, icu_collator::provider::Baked);
forward!(CollationDiacriticsV1, icu_collator::provider::Baked);
forward!(CollationJamoV1, icu_collator::provider::Baked);
forward!(CollationSpecialPrimariesV1, icu_collator::provider::Baked);
forward!(CollationReorderingV1, icu_collator::provider::Baked);
forward!(NormalizerNfdDataV1, icu_normalizer::provider::Baked);
forward!(NormalizerNfdTablesV1, icu_normalizer::provider::Baked);

/// Owned comparer for source-generated search data; ordinary collations still use Baked.
/// Case-sensitive ordering using portable compiled collation data.
pub struct CultureComparer {
    collator: Collator,
    search: bool,
    korean: bool,
}

impl CultureComparer {
    pub(super) fn new(locale: &Locale) -> Result<Self, DataError> {
        let requested = locale
            .extensions
            .unicode
            .keywords
            .get(&key!("co"))
            .map(ToString::to_string);
        let search = requested
            .as_deref()
            .is_some_and(|attribute| attribute.starts_with("search"));
        let mut effective = locale.clone();
        if search && !matches!(requested.as_deref(), Some("search" | "searchjl")) {
            // ICU's resource loader retries unknown search-prefixed types as search.
            effective.extensions.unicode.keywords.set(
                key!("co"),
                "search".parse().expect("known Unicode collation value"),
            );
        }
        let prefs: CollatorPreferences = (&effective).into();
        let korean = if search {
            let data_locale = CollationTailoringV1::make_locale(prefs.locale_preferences);
            let attribute = DataMarkerAttributes::from_str_or_panic(
                prefs
                    .collation_type
                    .as_ref()
                    .expect("normalized search type")
                    .as_str(),
            );
            let req = DataRequest {
                id: DataIdentifierBorrowed::for_marker_attributes_and_locale(
                    attribute,
                    &data_locale,
                ),
                ..Default::default()
            };
            let (entry, _) = find_entry(req)?;
            entry.locale.to_string() == "ko" && entry.attribute == "search"
        } else {
            false
        };
        Ok(Self {
            collator: Collator::try_new_unstable(
                &SearchDataProvider,
                prefs,
                CollatorOptions::default(),
            )?,
            search,
            korean,
        })
    }

    /// Compare strings under this captured culture (CompareOptions.None).
    #[must_use]
    pub fn compare(&self, left: &str, right: &str) -> Ordering {
        if self.search {
            let left = search_hangul::preprocess(left, self.korean);
            let right = search_hangul::preprocess(right, self.korean);
            self.collator.as_borrowed().compare(&left, &right)
        } else {
            self.collator.as_borrowed().compare(left, right)
        }
    }
}

/// Verify all root references used by official search data against the installed Baked root.
/// No source table is accepted merely on the basis of matching version labels.
#[cfg(test)]
fn verify_baked_root() -> Result<usize, String> {
    let source = generated_search_data::ROOT_COMPATIBILITY
        .data()
        .map_err(|error| error.to_string())?;
    let root = DataProvider::<CollationRootV1>::load(
        &icu_collator::provider::Baked,
        DataRequest::default(),
    )
    .map_err(|error| error.to_string())?
    .payload;
    let jamo = DataProvider::<CollationJamoV1>::load(
        &icu_collator::provider::Baked,
        DataRequest::default(),
    )
    .map_err(|error| error.to_string())?
    .payload;
    let dia = DataProvider::<CollationDiacriticsV1>::load(
        &icu_collator::provider::Baked,
        DataRequest::default(),
    )
    .map_err(|error| error.to_string())?
    .payload;
    if source.ce32s != root.get().ce32s
        || source.ces != root.get().ces
        || source.contexts != root.get().contexts
    {
        return Err("source root expansion/context arrays differ from Baked".to_owned());
    }
    if !jamo
        .get()
        .ce32s
        .iter()
        .eq(generated_search_data::ROOT_JAMO.iter().copied())
    {
        return Err("source root Jamo differs from Baked".to_owned());
    }
    if !dia
        .get()
        .secondaries
        .iter()
        .eq(generated_search_data::ROOT_DIACRITICS.iter().copied())
    {
        return Err("source root diacritics differ from Baked".to_owned());
    }
    let mut checked = 0;
    for codepoint in 0..=0x0010_ffff {
        // ICU4X SourceDataProvider explicitly clears this range during root trie conversion.
        if (0xac00..0xd7a4).contains(&codepoint) {
            continue;
        }
        if source.trie.get32(codepoint) != root.get().trie.get32(codepoint) {
            return Err(format!(
                "source/Baked root CE32 differs at U+{codepoint:04X}"
            ));
        }
        checked += 1;
    }
    Ok(checked)
}

/// Construct every imported locale/attribute pair, including Korean searchjl.
#[cfg(test)]
fn verify_all_entries() -> Result<usize, String> {
    for entry in entries().map_err(|error| error.to_string())? {
        let mut locale: Locale = entry
            .locale
            .to_string()
            .parse()
            .map_err(|error: icu_locale_core::ParseError| error.to_string())?;
        locale.extensions.unicode.keywords.set(
            key!("co"),
            entry.attribute.parse().expect("exported search attribute"),
        );
        let comparer = CultureComparer::new(&locale).map_err(|error| error.to_string())?;
        if comparer.compare("same", "same") != Ordering::Equal {
            return Err(format!(
                "imported {:?} {:?} changed self equality",
                entry.locale, entry.attribute
            ));
        }
    }
    Ok(entries().map_err(|error| error.to_string())?.len())
}

#[cfg(test)]
mod tests;
