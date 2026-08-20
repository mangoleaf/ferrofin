//! A [`fancy_regex::Regex`] fronted by a linear-time "can't possibly match" guard.
//!
//! The vendored Jellyfin patterns are ported verbatim, and several of them use
//! lookaround, so they have to run on `fancy-regex`'s backtracking engine. That
//! engine is worst-case exponential, and on a full media path (`.*(\\|\/)…`
//! prefixes over 60–150 character paths) the *failing* attempts — which are the
//! overwhelming majority, since a scan tries ~25 expressions per file and at
//! most one matches — cost tens to hundreds of microseconds each.
//!
//! [`GuardedRegex`] pairs the real pattern with a **relaxed** copy of itself in
//! which every lookaround assertion has been deleted. Deleting a zero-width
//! assertion can only ever *widen* the language a pattern accepts (an input the
//! original matched still satisfies every remaining obligation), so the relaxed
//! pattern accepts a superset. It contains no fancy constructs, so it compiles
//! on the linear-time [`regex`] engine — which means:
//!
//! * relaxed says **no match** → the real pattern provably cannot match, and we
//!   skip the backtracking engine entirely;
//! * relaxed says **match** → we run the real pattern exactly as before.
//!
//! Results are therefore byte-identical to running `fancy-regex` alone; only the
//! failing path gets cheaper. If the relaxed pattern fails to build for any
//! reason the guard is simply dropped and behaviour is unchanged.

use fancy_regex::{Captures, Error, Regex};

/// A compiled naming pattern with a linear-time rejection guard.
///
/// See the [module docs](self) for why the guard is sound.
#[derive(Debug, Clone)]
pub struct GuardedRegex {
    /// The relaxed (lookaround-free) superset pattern, when one could be built.
    guard: Option<regex::Regex>,
    /// The real, verbatim-ported pattern.
    inner: Regex,
}

impl GuardedRegex {
    /// Compiles `pattern`, building the rejection guard when possible.
    ///
    /// # Errors
    ///
    /// Returns the `fancy-regex` error if `pattern` is not a valid regex. Guard
    /// construction never fails the call: an unbuildable guard is just absent.
    #[allow(
        clippy::result_large_err,
        reason = "mirrors fancy_regex::Regex::new's own signature"
    )]
    pub fn new(pattern: &str) -> Result<Self, Error> {
        let inner = Regex::new(pattern)?;
        let guard = strip_lookaround(pattern).and_then(|relaxed| regex::Regex::new(&relaxed).ok());
        Ok(Self { guard, inner })
    }

    /// Returns the captures of the first match, or `None` when there is none.
    ///
    /// # Errors
    ///
    /// Propagates a `fancy-regex` runtime error (e.g. backtrack-limit exceeded).
    #[allow(
        clippy::result_large_err,
        reason = "mirrors fancy_regex::Regex::captures' own signature"
    )]
    pub fn captures<'t>(&self, text: &'t str) -> Result<Option<Captures<'t>>, Error> {
        if self.guard.as_ref().is_some_and(|g| !g.is_match(text)) {
            return Ok(None);
        }
        self.inner.captures(text)
    }

    /// Returns whether the pattern matches `text`.
    ///
    /// # Errors
    ///
    /// Propagates a `fancy-regex` runtime error (e.g. backtrack-limit exceeded).
    #[allow(
        clippy::result_large_err,
        reason = "mirrors fancy_regex::Regex::is_match's own signature"
    )]
    pub fn is_match(&self, text: &str) -> Result<bool, Error> {
        if self.guard.as_ref().is_some_and(|g| !g.is_match(text)) {
            return Ok(false);
        }
        self.inner.is_match(text)
    }

    /// Returns whether a rejection guard was built for this pattern.
    #[must_use]
    pub fn has_guard(&self) -> bool {
        self.guard.is_some()
    }
}

/// Deletes every lookaround group — `(?=…)`, `(?!…)`, `(?<=…)`, `(?<!…)` — from
/// `pattern`, along with any quantifier attached directly to it.
///
/// Returns `None` if the pattern's parentheses do not balance (in which case the
/// caller simply goes without a guard). Named groups (`(?<name>…)`) are left
/// alone; only `(?<=` and `(?<!` are lookbehind.
fn strip_lookaround(pattern: &str) -> Option<String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::with_capacity(pattern.len());
    let mut i = 0;
    let mut in_class = false;

    while i < chars.len() {
        let c = chars[i];

        if c == '\\' {
            out.push(c);
            if let Some(next) = chars.get(i + 1) {
                out.push(*next);
            }
            i += 2;
            continue;
        }

        if in_class {
            if c == ']' {
                in_class = false;
            }
            out.push(c);
            i += 1;
            continue;
        }

        if c == '[' {
            in_class = true;
            out.push(c);
            i += 1;
            continue;
        }

        if c == '(' && is_lookaround_at(&chars, i) {
            i = group_end(&chars, i)? + 1;
            while matches!(chars.get(i), Some('*' | '+' | '?')) {
                i += 1;
            }
            continue;
        }

        out.push(c);
        i += 1;
    }

    if in_class { None } else { Some(out) }
}

/// Whether the group opening at `i` is a lookaround assertion.
fn is_lookaround_at(chars: &[char], i: usize) -> bool {
    if chars.get(i + 1) != Some(&'?') {
        return false;
    }
    match chars.get(i + 2) {
        Some('=' | '!') => true,
        Some('<') => matches!(chars.get(i + 3), Some('=' | '!')),
        _ => false,
    }
}

/// Index of the `)` closing the group that opens at `start`.
fn group_end(chars: &[char], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = start;
    let mut in_class = false;

    while i < chars.len() {
        let c = chars[i];
        if c == '\\' {
            i += 2;
            continue;
        }
        if in_class {
            if c == ']' {
                in_class = false;
            }
            i += 1;
            continue;
        }
        match c {
            '[' => in_class = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }

    None
}

#[cfg(test)]
mod tests {
    use super::strip_lookaround;

    #[test]
    fn strips_negative_lookahead() {
        assert_eq!(
            strip_lookaround(r"[Ss](?![0-9]+[Ee])(\d+)").as_deref(),
            Some(r"[Ss](\d+)")
        );
    }

    #[test]
    fn strips_lookbehind_but_keeps_named_groups() {
        assert_eq!(
            strip_lookaround(r"(?<=a)(?<name>b)(?<!c)").as_deref(),
            Some(r"(?<name>b)")
        );
    }

    #[test]
    fn strips_quantifier_attached_to_assertion() {
        assert_eq!(strip_lookaround(r"a(?=b)*c").as_deref(), Some("ac"));
    }

    #[test]
    fn ignores_parens_and_brackets_inside_classes_and_escapes() {
        assert_eq!(
            strip_lookaround(r"[(?=x)]\(?=y\)(?!z)").as_deref(),
            Some(r"[(?=x)]\(?=y\)")
        );
    }

    #[test]
    fn nested_groups_inside_the_assertion_are_consumed() {
        assert_eq!(strip_lookaround(r"a(?!(b(c))d)e").as_deref(), Some("ae"));
    }

    #[test]
    fn unbalanced_pattern_yields_no_guard() {
        assert_eq!(strip_lookaround(r"a(?!bc"), None);
        assert_eq!(strip_lookaround(r"a[bc"), None);
    }
}
