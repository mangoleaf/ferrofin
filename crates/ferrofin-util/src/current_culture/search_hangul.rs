//! Canonical Hangul syllable decomposition for actual ICU search tailorings.
//! Modern Jamo use actual tailoring CE32s; erased Korean archaic equality
//! rules expand their source resets only under resolved ko/search.
use std::borrow::Cow;

#[path = "generated_korean_search_resets.rs"]
mod generated_korean_search_resets;
use generated_korean_search_resets::ERASED_KOREAN;

pub(super) fn preprocess(input: &str, korean: bool) -> Cow<'_, str> {
    let Some((start, _)) = input.char_indices().find(|(_, c)| {
        ('\u{ac00}'..='\u{d7a3}').contains(c)
            || (korean && ERASED_KOREAN.binary_search_by_key(c, |(c, _)| *c).is_ok())
    }) else {
        return Cow::Borrowed(input);
    };
    let mut output = String::with_capacity(input.len());
    output.push_str(&input[..start]);
    for character in input[start..].chars() {
        if ('\u{ac00}'..='\u{d7a3}').contains(&character) {
            let offset = u32::from(character) - 0xac00;
            for codepoint in [0x1100 + offset / 588, 0x1161 + (offset % 588) / 28] {
                output.push(char::from_u32(codepoint).expect("canonical Hangul component"));
            }
            if offset % 28 != 0 {
                output.push(
                    char::from_u32(0x11a7 + offset % 28).expect("canonical Hangul component"),
                );
            }
        } else if korean {
            match ERASED_KOREAN.binary_search_by_key(&character, |(c, _)| *c) {
                Ok(index) => output.push_str(ERASED_KOREAN[index].1),
                Err(_) => output.push(character),
            }
        } else {
            output.push(character);
        }
    }
    Cow::Owned(output)
}
