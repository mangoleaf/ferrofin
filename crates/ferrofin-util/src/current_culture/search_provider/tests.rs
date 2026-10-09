use super::*;
use crate::current_culture::comparer;

struct OracleRow {
    input: &'static str,
    valid: bool,
    signs: &'static [i8],
}

mod dotnet_oracle;

fn sign(order: Ordering) -> i8 {
    match order {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

#[test]
fn official_search_payloads_share_the_exact_compiled_root() {
    assert_eq!(verify_baked_root().unwrap(), 1_102_940);
    assert_eq!(verify_all_entries().unwrap(), 22);
}

#[test]
fn public_search_comparer_matches_the_full_dotnet_pair_oracle() {
    for row in dotnet_oracle::ROWS {
        let result = comparer(row.input);
        if !row.valid {
            assert!(
                result.is_err(),
                "source CompareInfo rejects {:?}",
                row.input
            );
            continue;
        }
        let comparer = result.unwrap();
        assert_eq!(row.signs.len(), dotnet_oracle::PAIRS.len());
        for ((left, right), expected) in dotnet_oracle::PAIRS.iter().zip(row.signs) {
            assert_eq!(
                sign(comparer.compare(left, right)),
                *expected,
                "culture {:?}, operands {left:?} and {right:?}",
                row.input
            );
        }
    }
}

fn canonical_syllable(character: char) -> String {
    let offset = u32::from(character) - 0xac00;
    let mut result = String::new();
    result.push(char::from_u32(0x1100 + offset / 588).unwrap());
    result.push(char::from_u32(0x1161 + (offset % 588) / 28).unwrap());
    if offset % 28 != 0 {
        result.push(char::from_u32(0x11a7 + offset % 28).unwrap());
    }
    result
}

#[test]
fn all_modern_syllables_preserve_canonical_and_search_equalities() {
    for culture in [
        "",
        "de-DE",
        "sv-SE",
        "ko-KR",
        "de-DE_search",
        "sv-SE_search",
        "en-US_search",
        "ko-KR_search",
        "ko-KR_searchjl",
    ] {
        let comparer = comparer(culture).unwrap();
        for codepoint in 0xac00..=0xd7a3 {
            let character = char::from_u32(codepoint).unwrap();
            let canonical = canonical_syllable(character);
            assert_eq!(
                comparer.compare(&character.to_string(), &canonical),
                Ordering::Equal,
                "{culture} canonical U+{codepoint:04X}"
            );
            if culture.ends_with("_search") {
                let mut equivalent = String::new();
                for component in canonical.chars() {
                    if let Some((_, reset)) = dotnet_oracle::ROOT_SEARCH_RESETS
                        .iter()
                        .find(|(c, _)| *c == component)
                    {
                        equivalent.push_str(reset);
                    } else {
                        equivalent.push(component);
                    }
                }
                assert_eq!(
                    comparer.compare(&character.to_string(), &equivalent),
                    Ordering::Equal,
                    "{culture} search U+{codepoint:04X}"
                );
            }
        }
    }
}

#[test]
fn actual_resolved_korean_search_controls_erased_archaic_rules() {
    let search = CultureComparer::new(&"ko-KR-u-co-search".parse().unwrap()).unwrap();
    let leading = CultureComparer::new(&"ko-KR-u-co-searchjl".parse().unwrap()).unwrap();
    let ordinary = CultureComparer::new(&"ko-KR".parse().unwrap()).unwrap();
    assert!(search.search && search.korean);
    assert!(leading.search && !leading.korean);
    assert!(!ordinary.search && !ordinary.korean);
    assert_eq!(search.compare("/ᄔ.jpg", "/ᄂᄂ.jpg"), Ordering::Equal);
    assert_ne!(leading.compare("/ᄔ.jpg", "/ᄂᄂ.jpg"), Ordering::Equal);
    assert_ne!(ordinary.compare("/ᄔ.jpg", "/ᄂᄂ.jpg"), Ordering::Equal);
    assert_eq!(leading.compare("/ᄀᄀ.jpg", "/ᄁ.jpg"), Ordering::Equal);
    assert_eq!(leading.compare("/ᄀ.jpg", "/ᆨ.jpg"), Ordering::Greater);
}

#[test]
fn unknown_search_prefix_and_missing_leading_tailoring_retry_search() {
    for locale in ["de-DE-u-co-searchxx", "de-DE-u-co-searchjl"] {
        let comparer = CultureComparer::new(&locale.parse().unwrap()).unwrap();
        assert!(comparer.search && !comparer.korean);
        assert_eq!(comparer.compare("ae", "ä"), Ordering::Less);
        assert_eq!(comparer.compare("/ᆰ.jpg", "/ᆯᆨ.jpg"), Ordering::Equal);
    }
    let ordinary = CultureComparer::new(&"de-DE-u-co-phoneboo".parse().unwrap()).unwrap();
    assert!(!ordinary.search);
    assert_eq!(ordinary.compare("ae", "ä"), Ordering::Greater);
}

#[test]
fn invalid_source_trie_is_rejected_before_constructing_a_payload() {
    let mut invalid = generated_search_data::SOURCES[0];
    invalid.index = &[];
    assert!(invalid.data().is_err());
    let locale: DataLocale = "de".parse().unwrap();
    let missing = DataMarkerAttributes::from_str_or_panic("missing");
    assert!(
        find_entry(DataRequest {
            id: DataIdentifierBorrowed::for_marker_attributes_and_locale(missing, &locale),
            ..Default::default()
        })
        .is_err()
    );
}
