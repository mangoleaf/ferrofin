//! The current formatting/sort culture captured by asynchronous NFO work.
//!
//! .NET's ExecutionContext carries CurrentCulture across awaits and Task.Run.
//! Rust's worker threads do not: install a captured culture only while polling
//! its future, restoring the thread's previous context before it is parked.
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;
use std::task::{Context, Poll};

use icu_locale_core::{Locale, extensions::unicode::key};

mod search_provider;

/// A portable comparer for the captured formatting/sort culture.
pub use search_provider::CultureComparer;

thread_local! {
    static CURRENT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Invalid culture names and unavailable ICU collation data.
#[derive(Debug, thiserror::Error)]
pub enum CultureError {
    /// A culture name cannot be represented as an ICU locale.
    #[error("invalid culture: {0}")]
    Invalid(String),
    /// The bundled ICU data cannot construct the requested comparer.
    #[error("culture comparer: {0}")]
    Collator(String),
}

/// The culture inherited by work started in the current asynchronous context.
#[must_use]
pub fn capture() -> String {
    CURRENT.with(|current| {
        current.borrow().clone().unwrap_or_else(|| {
            static SYSTEM: OnceLock<String> = OnceLock::new();
            SYSTEM
                .get_or_init(|| {
                    system_locale(
                        std::env::var("LC_ALL").ok().as_deref(),
                        std::env::var("LC_MESSAGES").ok().as_deref(),
                        std::env::var("LANG").ok().as_deref(),
                    )
                })
                .clone()
        })
    })
}

/// Canonicalizes the current-culture comparison identity, retaining alternate
/// collation rather than discarding it through CultureInfo.Name.
///
/// # Errors
/// Returns an error for an invalid locale identifier.
pub fn canonical_name(name: &str) -> Result<String, CultureError> {
    Ok(parse_culture(name)?.identity)
}

/// The public CultureInfo.Name used by the startup Content-Language default.
/// Specific alternate-collation cultures expose their base name here, while
/// neutral names such as the dashboard's es_419 retain the underscore suffix.
///
/// # Errors
/// Returns an error for an invalid culture identifier.
pub fn display_name(name: &str) -> Result<String, CultureError> {
    Ok(parse_culture(name)?.name)
}

/// The parent used by the pinned supported-request catalog. An alternate
/// collation has its unsorted base as parent; ordinary extension keywords
/// disappear with the removed base component.
///
/// # Errors
/// Returns an error for an invalid culture identifier.
pub fn parent_name(name: &str) -> Result<Option<String>, CultureError> {
    Ok(parse_culture(name)?.parent)
}

/// Culture-sensitive, case-sensitive string ordering (CompareOptions.None).
///
/// # Errors
/// Returns an error if the culture name or bundled collation data is invalid.
pub fn comparer(name: &str) -> Result<CultureComparer, CultureError> {
    let culture = parse_culture(name)?;
    // .NET constructs und alternate cultures, but their normalized identities
    // start with '_'/'-' and its later CompareInfo name lookup throws.
    if culture.identity.starts_with(['_', '-']) {
        return Err(CultureError::Invalid(culture.identity));
    }
    CultureComparer::new(&culture.locale).map_err(|error| CultureError::Collator(error.to_string()))
}

struct ParsedCulture {
    identity: String,
    name: String,
    parent: Option<String>,
    locale: Locale,
}

fn parse_culture(name: &str) -> Result<ParsedCulture, CultureError> {
    let invalid = || CultureError::Invalid(name.into());
    if name.is_empty() || name.eq_ignore_ascii_case("und") || name.eq_ignore_ascii_case("root") {
        return Ok(ParsedCulture {
            identity: String::new(),
            name: String::new(),
            parent: None,
            locale: Locale::UNKNOWN,
        });
    }
    // Source .NET keeps these legacy public names; the actual pinned catalog
    // has neither name, so requested names fall through the unspecific parent.
    if name.eq_ignore_ascii_case("zh-CHS") || name.eq_ignore_ascii_case("zh-CHT") {
        let name = if name.eq_ignore_ascii_case("zh-CHS") {
            "zh-CHS"
        } else {
            "zh-CHT"
        };
        return Ok(ParsedCulture {
            identity: name.into(),
            name: name.into(),
            parent: Some("zh".into()),
            locale: "zh".parse().map_err(|_| invalid())?,
        });
    }
    if let Some((base, sort)) = name.split_once('_') {
        // CultureData.Icu accepts one underscore as an alternate sort, not as
        // a region separator. Normalizing every underscore to '-' changes the
        // actual dashboard values ar_SA/es_419/es_DO/ur_PK.
        if sort.is_empty()
            || sort.starts_with('-')
            || sort.ends_with('-')
            || sort.contains("--")
            || !sort
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(invalid());
        }
        let locale = locale_for_name(base)?;
        let base_name = extension_name(&locale, base);
        let parent = if base.to_ascii_lowercase().contains("-u-") {
            base_parent(&locale)
        } else {
            (!locale.id.is_unknown()).then(|| public_base_name(&locale, base))
        };
        return alternate_culture(locale, sort, base_name, parent);
    }
    let mut locale = locale_for_name(name)?;
    if let Some(sort) = locale.extensions.unicode.keywords.remove(key!("co")) {
        // ICU's canonical keyword names are longer than several BCP47 aliases.
        // .NET NormalizeCultureName truncates the resulting alternate suffix
        // to eight characters before CompareInfo later opens that identifier.
        let sort = sort.to_string();
        let legacy = match sort.as_str() {
            "phonebk" => "phonebook",
            "dict" => "dictionary",
            "trad" => "traditional",
            "gb2312" => "gb2312han",
            other => other,
        };
        let co_first = name.to_ascii_lowercase().contains("-u-co-");
        let (base_name, parent) = if co_first {
            let parent = locale.id.region.map(|_| public_base_name(&locale, name));
            // When co is first, .NET's retained-extension span is empty; later
            // ca/nu/kn keys are not part of the resulting comparison identity.
            locale.extensions = icu_locale_core::extensions::Extensions::default();
            (public_base_name(&locale, name), parent)
        } else {
            // .NET retains the original nonco-first extension span, including
            // keyword order, rather than ICU4X's sorted keyword serialization.
            (extension_name(&locale, name), base_parent(&locale))
        };
        return alternate_culture(locale, legacy, base_name, parent);
    }
    let identity = extension_name(&locale, name);
    let parent = base_parent(&locale);
    Ok(ParsedCulture {
        name: identity.clone(),
        identity,
        parent,
        locale,
    })
}

// ICU accepts its reserved root name, which ICU4X's BCP47 language parser does
// not. Keep the public source spelling separately from the actual root locale.
fn locale_for_name(name: &str) -> Result<Locale, CultureError> {
    let lower = name.to_ascii_lowercase();
    if lower == "root" {
        return Ok(Locale::UNKNOWN);
    }
    let normalized = lower
        .strip_prefix("root-")
        .map(|suffix| format!("und-{suffix}"));
    normalized
        .as_deref()
        .unwrap_or(name)
        .parse()
        .map_err(|_| CultureError::Invalid(name.into()))
}

fn alternate_culture(
    mut locale: Locale,
    sort: &str,
    base: String,
    parent: Option<String>,
) -> Result<ParsedCulture, CultureError> {
    // The .NET native comparer falls back to the base tailoring for unknown
    // ICU keyword names, including the measured phoneb/phoneboo truncations.
    // Retain registered legacy names without inventing Windows alias mappings.
    let sort: String = sort.chars().take(8).collect();
    let identity = format!("{base}_{sort}");
    let name = if locale.id.region.is_none() {
        identity.clone()
    } else {
        base
    };
    // Reparsing a captured nonco-first identity may still contain co=phonebk
    // in the base. The appended legacy suffix overrides it, even when that
    // suffix is unknown/truncated and the native comparer uses base tailoring.
    locale.extensions.unicode.keywords.remove(key!("co"));
    let lower_sort = sort.to_ascii_lowercase();
    if lower_sort.starts_with("search")
        || matches!(
            lower_sort.as_str(),
            "compat"
                | "ducet"
                | "emoji"
                | "eor"
                | "phonetic"
                | "pinyin"
                | "search"
                | "searchjl"
                | "standard"
                | "stroke"
                | "unihan"
                | "zhuyin"
        )
    {
        let actual_sort = if lower_sort.starts_with("search") && lower_sort != "searchjl" {
            "search"
        } else {
            &lower_sort
        };
        let value = actual_sort
            .parse()
            .map_err(|_| CultureError::Invalid(identity.clone()))?;
        locale.extensions.unicode.keywords.set(key!("co"), value);
    }
    Ok(ParsedCulture {
        identity,
        name,
        parent,
        locale,
    })
}

fn public_base_name(locale: &Locale, original: &str) -> String {
    let lower = original.to_ascii_lowercase();
    if lower == "root" || lower.starts_with("root-") {
        locale.id.to_string().replacen("und", "root", 1)
    } else if locale.id.is_unknown() {
        String::new()
    } else {
        locale.id.to_string()
    }
}

fn extension_name(locale: &Locale, original: &str) -> String {
    let lower = original.to_ascii_lowercase();
    let base = public_base_name(locale, original);
    // CultureData.NormalizeCultureName retains the original extension span.
    // The scoped request catalog uses plain names; this branch matters for a
    // saved default that carries several ordered Unicode keywords.
    lower
        .find("-u-")
        .or_else(|| lower.find("-t-"))
        .map_or(base.clone(), |start| {
            format!("{base}{}", &original[start..])
        })
}

fn base_parent(locale: &Locale) -> Option<String> {
    if locale.id.language.is_unknown() {
        return None;
    }
    locale
        .id
        .to_string()
        .rsplit_once('-')
        .map(|(parent, _)| parent.to_owned())
}

/// Polls a future under a captured culture, including after thread migration.
pub fn scope<F: Future>(culture: String, future: F) -> CultureFuture<F> {
    CultureFuture {
        culture,
        future: Box::pin(future),
    }
}

/// A future carrying the culture of the caller that started its work.
pub struct CultureFuture<F> {
    culture: String,
    future: Pin<Box<F>>,
}

struct Restore(Option<String>);
impl Drop for Restore {
    fn drop(&mut self) {
        CURRENT.with(|current| *current.borrow_mut() = self.0.take());
    }
}

impl<F: Future> Future for CultureFuture<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let previous = CURRENT.with(|current| current.replace(Some(this.culture.clone())));
        let _restore = Restore(previous);
        this.future.as_mut().poll(context)
    }
}

fn system_locale(lc_all: Option<&str>, lc_messages: Option<&str>, lang: Option<&str>) -> String {
    // ICU's Linux default locale checks LC_ALL, LC_MESSAGES, then LANG. A
    // present empty variable wins. C/POSIX/en_US_POSIX map to .NET invariant.
    let raw = lc_all.or(lc_messages).or(lang).unwrap_or("C");
    let raw = raw.split('@').next().unwrap_or_default();
    let raw = raw.split('.').next().unwrap_or_default();
    if matches!(raw, "" | "C" | "POSIX" | "en_US_POSIX") {
        return String::new();
    }
    canonical_name(&raw.replace('_', "-")).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::Waker;

    struct Once(bool);
    impl Future for Once {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                Poll::Pending
            }
        }
    }

    #[test]
    fn culture_survives_pending_thread_migration_and_restores_each_poll() {
        let original = capture();
        let mut future = Box::pin(scope("sv-SE".into(), async {
            let first = capture();
            Once(false).await;
            assert_eq!(capture(), first);
            capture()
        }));
        let waker = Waker::noop();
        assert!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(waker))
                .is_pending()
        );
        assert_eq!(capture(), original);
        std::thread::spawn(move || {
            let original = capture();
            let waker = Waker::noop();
            assert_eq!(
                future.as_mut().poll(&mut Context::from_waker(waker)),
                Poll::Ready("sv-SE".into())
            );
            assert_eq!(capture(), original);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn nested_scope_and_panics_restore_the_callers_culture() {
        let original = capture();
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut future = Box::pin(scope("en-US".into(), async {
            assert_eq!(capture(), "en-US");
            assert_eq!(scope("sv".into(), async { capture() }).await, "sv");
            assert_eq!(capture(), "en-US");
            let mut failed = Box::pin(scope("de-DE".into(), async {
                panic!("fixture");
            }));
            let waker = Waker::noop();
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    failed.as_mut().poll(&mut Context::from_waker(waker))
                }))
                .is_err()
            );
            assert_eq!(capture(), "en-US");
        }));
        assert!(future.as_mut().poll(&mut context).is_ready());
        assert_eq!(capture(), original);
    }

    #[test]
    fn current_culture_uses_language_tailoring_and_stable_canonical_equalities() {
        use std::cmp::Ordering;
        let en = comparer("en-US").unwrap();
        let sv = comparer("sv-SE").unwrap();
        assert_eq!(en.compare("/ä.jpg", "/z.jpg"), Ordering::Less);
        assert_eq!(sv.compare("/ä.jpg", "/z.jpg"), Ordering::Greater);
        assert_eq!(en.compare("/é.jpg", "/e\u{301}.jpg"), Ordering::Equal);
        assert_eq!(en.compare("/A.jpg", "/a.jpg"), Ordering::Greater);
        assert!(comparer("bad___culture").is_err());
        assert!(comparer("").is_ok());
    }

    #[test]
    fn process_culture_uses_lc_messages_precedence_and_invariant_posix() {
        assert_eq!(
            system_locale(Some("sv_SE.UTF-8"), Some("en_US.UTF-8"), Some("fr_FR")),
            "sv-SE"
        );
        assert_eq!(
            system_locale(None, Some("de_DE.UTF-8"), Some("sv_SE")),
            "de-DE"
        );
        assert_eq!(system_locale(None, None, Some("en_US.UTF-8")), "en-US");
        for locale in ["", "C", "C.UTF-8", "POSIX", "en_US_POSIX"] {
            assert_eq!(system_locale(Some(locale), None, Some("sv_SE")), "");
        }
        assert_eq!(system_locale(None, None, None), "");
        assert_eq!(system_locale(None, None, Some("bad___culture")), "");
        assert_eq!(canonical_name("").unwrap(), "");
        assert_eq!(
            parent_name("sv-SE-u-ca-gregory").unwrap(),
            Some("sv".into())
        );
        assert_eq!(parent_name("sv-u-ca-gregory").unwrap(), None);
        assert_eq!(
            parent_name("zh-Hans-CN-u-ca-chinese").unwrap(),
            Some("zh-Hans".into())
        );
        assert_eq!(parent_name("").unwrap(), None);
        assert!(parent_name("bad___culture").is_err());
    }
    #[test]
    fn raw_dashboard_names_and_alternate_collation_match_dotnet_oracle() {
        for (name, parent) in [
            ("es_419", "es"),
            ("ar_SA", "ar"),
            ("es_DO", "es"),
            ("ur_PK", "ur"),
            ("en_US", "en"),
            ("sv_SE", "sv"),
        ] {
            assert_eq!(canonical_name(name).unwrap(), name);
            assert_eq!(display_name(name).unwrap(), name);
            assert_eq!(parent_name(name).unwrap().as_deref(), Some(parent));
        }
        assert_eq!(
            comparer("sv_SE").unwrap().compare("/ä.jpg", "/z.jpg"),
            std::cmp::Ordering::Greater
        );
        for (input, identity, public) in [
            ("de-DE_phoneb", "de-DE_phoneb", "de-DE"),
            ("de-DE-u-co-phonebk", "de-DE_phoneboo", "de-DE"),
            ("en-US-u-co-phonebk", "en-US_phoneboo", "en-US"),
        ] {
            assert_eq!(canonical_name(input).unwrap(), identity);
            assert_eq!(display_name(input).unwrap(), public);
            assert_eq!(parent_name(input).unwrap().as_deref(), Some(public));
            assert_eq!(
                comparer(input).unwrap().compare("/ä.jpg", "/ae.jpg"),
                std::cmp::Ordering::Less,
                "the reference's truncated collation resolves to base ordering"
            );
        }
        for name in ["zh-CHS", "zh-CHT"] {
            assert_eq!(canonical_name(name).unwrap(), name);
            assert_eq!(display_name(name).unwrap(), name);
            assert_eq!(parent_name(name).unwrap().as_deref(), Some("zh"));
        }
        for name in ["bad___culture", "bad!culture", "es_", "es__419", "es_-419"] {
            assert!(canonical_name(name).is_err());
        }
    }

    #[test]
    fn combined_unicode_order_and_equal_keys_match_actual_current_culture() {
        let original = [
            "/ä.jpg",
            "/z.jpg",
            "/é.jpg",
            "/e\u{301}.jpg",
            "/A.jpg",
            "/a.jpg",
        ];
        for (name, expected) in [
            (
                "en-US",
                [
                    "/a.jpg",
                    "/A.jpg",
                    "/ä.jpg",
                    "/é.jpg",
                    "/e\u{301}.jpg",
                    "/z.jpg",
                ],
            ),
            (
                "sv-SE",
                [
                    "/a.jpg",
                    "/A.jpg",
                    "/é.jpg",
                    "/e\u{301}.jpg",
                    "/z.jpg",
                    "/ä.jpg",
                ],
            ),
        ] {
            let comparer = comparer(name).unwrap();
            let mut sorted = original;
            sorted.sort_by(|left, right| comparer.compare(left, right));
            assert_eq!(sorted, expected);
        }
    }
    #[test]
    fn mixed_keywords_and_reparsed_captured_identities_match_dotnet_oracle() {
        let original = ["/ä.jpg", "/z.jpg", "/ae.jpg", "/A.jpg", "/a.jpg"];
        let rows = [
            (
                "fr-CA-u-nu-latn-co-phonebk",
                "fr-CA-u-nu-latn-co-phonebk_phoneboo",
                "fr-CA-u-nu-latn-co-phonebk",
                Some("fr"),
                ["/a.jpg", "/A.jpg", "/ä.jpg", "/ae.jpg", "/z.jpg"],
            ),
            (
                "fr-CA-u-co-phonebk-nu-latn",
                "fr-CA_phoneboo",
                "fr-CA",
                Some("fr-CA"),
                ["/a.jpg", "/A.jpg", "/ä.jpg", "/ae.jpg", "/z.jpg"],
            ),
            (
                "sv-SE-u-ca-gregory-co-phonebk",
                "sv-SE-u-ca-gregory-co-phonebk_phoneboo",
                "sv-SE-u-ca-gregory-co-phonebk",
                Some("sv"),
                ["/a.jpg", "/A.jpg", "/ae.jpg", "/z.jpg", "/ä.jpg"],
            ),
            (
                "sv-SE-u-co-phonebk-ca-gregory",
                "sv-SE_phoneboo",
                "sv-SE",
                Some("sv-SE"),
                ["/a.jpg", "/A.jpg", "/ae.jpg", "/z.jpg", "/ä.jpg"],
            ),
            (
                "sv-u-ca-gregory-co-phonebk",
                "sv-u-ca-gregory-co-phonebk_phoneboo",
                "sv-u-ca-gregory-co-phonebk_phoneboo",
                None,
                ["/a.jpg", "/A.jpg", "/ae.jpg", "/z.jpg", "/ä.jpg"],
            ),
            (
                "sv-u-co-phonebk-ca-gregory",
                "sv_phoneboo",
                "sv_phoneboo",
                None,
                ["/a.jpg", "/A.jpg", "/ae.jpg", "/z.jpg", "/ä.jpg"],
            ),
            (
                "de-DE-u-co-search",
                "de-DE_search",
                "de-DE",
                Some("de-DE"),
                ["/a.jpg", "/A.jpg", "/ae.jpg", "/ä.jpg", "/z.jpg"],
            ),
            (
                "sv-SE_search",
                "sv-SE_search",
                "sv-SE",
                Some("sv-SE"),
                ["/a.jpg", "/A.jpg", "/ae.jpg", "/z.jpg", "/ä.jpg"],
            ),
            (
                "de-DE-u-kn-true-co-phonebk",
                "de-DE-u-kn-true-co-phonebk_phoneboo",
                "de-DE-u-kn-true-co-phonebk",
                Some("de"),
                ["/a.jpg", "/A.jpg", "/ä.jpg", "/ae.jpg", "/z.jpg"],
            ),
            (
                "de-DE-u-co-phonebk-kn-true",
                "de-DE_phoneboo",
                "de-DE",
                Some("de-DE"),
                ["/a.jpg", "/A.jpg", "/ä.jpg", "/ae.jpg", "/z.jpg"],
            ),
            (
                "ur_PK",
                "ur_PK",
                "ur_PK",
                Some("ur"),
                ["/a.jpg", "/A.jpg", "/ä.jpg", "/ae.jpg", "/z.jpg"],
            ),
        ];
        for (input, identity, public, parent, expected) in rows {
            assert_eq!(canonical_name(input).unwrap(), identity, "{input}");
            assert_eq!(display_name(input).unwrap(), public, "{input}");
            assert_eq!(parent_name(input).unwrap().as_deref(), parent, "{input}");
            for captured in [input, identity] {
                let comparer = comparer(captured).unwrap();
                let mut sorted = original;
                sorted.sort_by(|left, right| comparer.compare(left, right));
                assert_eq!(sorted, expected, "comparer for {captured}");
            }
        }
    }
    #[test]
    fn root_und_metadata_and_comparison_failures_match_staged_dotnet_oracle() {
        // Framework10.0.12 staged oracle: preserve construction/public metadata
        // separately from lazy CompareInfo failures for normalized und names.
        let rows = [
            ("und-u-co-searchjl", "_searchjl", "_searchjl", ""),
            ("und", "", "", ""),
            ("und_search", "_search", "_search", ""),
            ("und-u-co-search", "_search", "_search", ""),
            ("und_searchjl", "_searchjl", "_searchjl", ""),
            ("root", "", "", ""),
            ("root_search", "root_search", "root_search", ""),
            ("root-u-co-search", "root_search", "root_search", ""),
            ("und-US", "und-US", "und-US", ""),
            ("und-001", "und-001", "und-001", ""),
            ("de-DE_searchxx", "de-DE_searchxx", "de-DE", "de-DE"),
            ("de-DE-u-co-searchxx", "de-DE_searchxx", "de-DE", "de-DE"),
            ("ROOT", "", "", ""),
            ("UND", "", "", ""),
            ("root_searchjl", "root_searchjl", "root_searchjl", ""),
            ("root-u-co-searchjl", "root_searchjl", "root_searchjl", ""),
            ("und-u-nu-latn", "-u-nu-latn", "-u-nu-latn", ""),
            ("root-u-nu-latn", "root-u-nu-latn", "root-u-nu-latn", ""),
            (
                "und-u-nu-latn-co-search",
                "-u-nu-latn-co-search_search",
                "-u-nu-latn-co-search_search",
                "",
            ),
            (
                "root-u-nu-latn-co-search",
                "root-u-nu-latn-co-search_search",
                "root-u-nu-latn-co-search_search",
                "",
            ),
            ("und-Latn", "und-Latn", "und-Latn", ""),
            ("und-Latn-US", "und-Latn-US", "und-Latn-US", ""),
            ("root-US", "root-US", "root-US", ""),
            ("root-US_search", "root-US_search", "root-US", "root-US"),
            ("UND_US", "_US", "_US", ""),
        ];
        for (input, identity, name, parent) in rows {
            assert_eq!(
                canonical_name(input).unwrap(),
                identity,
                "identity: {input}"
            );
            assert_eq!(display_name(input).unwrap(), name, "public name: {input}");
            assert_eq!(
                parent_name(input).unwrap().unwrap_or_default(),
                parent,
                "parent: {input}"
            );
            if identity.starts_with(['_', '-']) {
                assert!(
                    comparer(input).is_err(),
                    "lazy comparison must fail: {input}"
                );
                assert!(
                    canonical_name(identity).is_err(),
                    "normalized lookup must fail: {input}"
                );
            }
        }
    }

    #[test]
    fn search_prefixes_preserve_identity_and_reach_registered_search_preferences() {
        for input in ["de-DE_searchxx", "de-DE-u-co-searchxx"] {
            let parsed = parse_culture(input).unwrap();
            assert_eq!(parsed.identity, "de-DE_searchxx");
            assert_eq!(
                parsed
                    .locale
                    .extensions
                    .unicode
                    .keywords
                    .get(&key!("co"))
                    .unwrap()
                    .to_string(),
                "search"
            );
        }
        let jl = parse_culture("ko-KR_searchjl").unwrap();
        assert_eq!(
            jl.locale
                .extensions
                .unicode
                .keywords
                .get(&key!("co"))
                .unwrap()
                .to_string(),
            "searchjl"
        );
    }
}
