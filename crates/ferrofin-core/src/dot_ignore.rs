//! Port of `DotIgnoreIgnoreRule` (`Emby.Server.Implementations/Library/
//! DotIgnoreIgnoreRule.cs`): a `.ignore` file in a folder, or in the nearest
//! folder above it that has one, hides paths from the scanner and the library
//! monitor. An empty file, or one without a valid line, hides everything
//! beneath it; otherwise each line is a gitignore-style pattern.
//!
//! Patterns are matched as upstream's `Ignore` 0.2.1 package matches them:
//! each line becomes a case-insensitive regex tested against the path's full
//! absolute form (with a trailing `/` for a folder), not against the path
//! relative to the `.ignore` folder. So `Extras/` and `*.part` hide matches at
//! any depth, while an anchored `/Extras` never matches.

use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use regex::{Regex, RegexBuilder};

/// One `.ignore` line (`Ignore.IgnoreRule`).
struct Rule {
    negate: bool,
    /// `None` for a blank line or a comment, which matches nothing.
    regex: Option<Regex>,
}

impl Rule {
    /// Parses `pattern`; `None` when its regex does not compile (upstream's
    /// `RegexParseException`, a line that is skipped).
    fn parse(pattern: &str) -> Option<Self> {
        if pattern.trim().is_empty() || pattern.starts_with('#') {
            return Some(Self {
                negate: false,
                regex: None,
            });
        }
        let (negate, pattern) = if pattern.starts_with("\\!") || pattern.starts_with("\\#") {
            (false, &pattern[1..])
        } else if let Some(rest) = pattern.strip_prefix('!') {
            (true, rest)
        } else {
            (false, pattern)
        };
        let regex = RegexBuilder::new(&dotnet_braces(&to_regex(pattern)))
            .case_insensitive(true)
            .build()
            .ok()?;
        Some(Self {
            negate,
            regex: Some(regex),
        })
    }
}

/// The rules of one `.ignore` file (`Ignore.Ignore`).
struct IgnoreRules(Vec<Rule>);

impl IgnoreRules {
    /// The valid rules among `file`'s `lines`; `None` when none is valid,
    /// which upstream treats as an empty file (everything is ignored).
    fn parse<'a>(lines: impl IntoIterator<Item = &'a str>, file: &Path) -> Option<Self> {
        let rules: Vec<Rule> = lines
            .into_iter()
            .filter_map(|line| {
                let rule = Rule::parse(line);
                if rule.is_none() {
                    tracing::debug!(path = %file.display(), line, ".ignore line is not a valid pattern; skipped");
                }
                rule
            })
            .collect();
        (!rules.is_empty()).then_some(Self(rules))
    }

    /// `IsPathIgnored`: the rules in order, where a negation only takes back
    /// what an earlier rule ignored.
    fn is_ignored(&self, path: &str) -> bool {
        self.0.iter().fold(false, |ignored, rule| {
            let matches = || rule.regex.as_ref().is_some_and(|r| r.is_match(path));
            if rule.negate {
                ignored && !matches()
            } else {
                ignored || matches()
            }
        })
    }
}

/// `IgnoreRule`'s replacer chain, in its order: a gitignore pattern to the
/// .NET regex `Ignore` 0.2.1 builds from it. `Ending` is left out: it uses
/// the same expression as `NoTrailingSlash`, whose output ends in `$`, so it
/// never matches after it.
fn to_regex(pattern: &str) -> String {
    // TrailingSpaces (`\\?\s+$`) and EscapedSpaces (`\\\s`).
    let trimmed = pattern.trim_end();
    let mut p = if trimmed.len() < pattern.len() && trimmed.ends_with('\\') {
        format!("{} ", &trimmed[..trimmed.len() - 1])
    } else {
        trimmed.to_owned()
    };
    p = escaped_spaces(&p);
    // LiteralDot, LiteralPlus, and QuestionMark: its `(?!\\)\?` looks ahead
    // at the `?` itself, so every `?` is replaced.
    p = p
        .replace('.', r"\.")
        .replace('+', r"\+")
        .replace('?', "[^/]");
    // NoSlash.
    if !p.contains('/') {
        p = format!("(^|/){p}");
    }
    p = non_leading_single_star(&p);
    // LeadingSingleStar.
    if p.starts_with('*') && !p[1..].starts_with('*') {
        p = format!(".*{}", &p[1..]);
    }
    // LeadingDoubleStar.
    if let Some(rest) = p.strip_prefix("**/").or_else(|| p.strip_prefix("**")) {
        p = format!(".*{rest}");
    }
    p = middle_double_star(&p);
    // LeadingSlash.
    if let Some(rest) = p.strip_prefix('/') {
        p = format!("^{rest}");
    }
    // TrailingDoubleStar, then OtherDoubleStar.
    if let Some(rest) = p.strip_suffix("**") {
        p = format!("{rest}.*$");
    }
    p = p.replace("**", "[^/]*");
    // MiddleSlash (`^([^/\^]+/[^/]+)`): a leading run free of `/` and `^`,
    // then a `/` and something other than `/`.
    if let Some(at) = p.find(['/', '^'])
        && at > 0
        && p[at..].starts_with('/')
        && p[at + 1..].chars().next().is_some_and(|c| c != '/')
    {
        p = format!("^{p}");
    }
    // TrailingSlash (`^([^/]+)/$`).
    if let Some(name) = p.strip_suffix('/')
        && !name.is_empty()
        && !name.contains('/')
    {
        p = format!("(/|^){name}/");
    }
    // NoTrailingSlash (`([^/$]+)$`).
    if !p.ends_with(['/', '$']) && !p.is_empty() {
        p.push_str("(/.*)?$");
    }
    p
}

/// EscapedSpaces (`\\\s` → a space).
fn escaped_spaces(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    let mut chars = p.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek().is_some_and(|next| next.is_whitespace()) {
            chars.next();
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

/// NonLeadingSingleStar (`(?<!^)(?<!\*)\*(?!\*)` → `[^/]*`): a lone `*`
/// anywhere but first.
fn non_leading_single_star(p: &str) -> String {
    let chars: Vec<char> = p.chars().collect();
    let mut out = String::with_capacity(p.len());
    for (i, &c) in chars.iter().enumerate() {
        let lone = c == '*'
            && i > 0
            && chars[i - 1] != '*'
            && chars.get(i + 1).is_none_or(|&next| next != '*');
        if lone {
            out.push_str("[^/]*");
        } else {
            out.push(c);
        }
    }
    out
}

/// MiddleDoubleStar (`(?<=/)\*\*/` → `(.*/)?`).
fn middle_double_star(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    let mut i = 0;
    while i < p.len() {
        if p[i..].starts_with("**/") && p[..i].ends_with('/') {
            out.push_str("(.*/)?");
            i += 3;
        } else {
            let c = p[i..].chars().next().unwrap_or_default();
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

/// .NET reads a `{` that does not open a `{n}`, `{n,}` or `{n,m}`
/// quantifier, and any lone `}`, as a literal, where the `regex` crate
/// rejects them; escape those so a line like `*{tmdb-603}*` keeps its
/// meaning. Escapes and character classes are copied as they are.
// ponytail: other .NET/Rust syntax gaps (`[[`, `&&` in a class) are left;
// translate them here if a real `.ignore` needs one.
fn dotnet_braces(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    let mut chars = p.char_indices().peekable();
    let mut in_class = false;
    let mut class_start = 0;
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                out.push(c);
                if let Some((_, next)) = chars.next() {
                    out.push(next);
                }
            }
            '[' if !in_class => {
                in_class = true;
                class_start = i + 1;
                out.push(c);
            }
            ']' if in_class
                && i != class_start
                && !(i == class_start + 1 && p[class_start..].starts_with('^')) =>
            {
                in_class = false;
                out.push(c);
            }
            '{' if !in_class => match quantifier_len(&p[i..]) {
                Some(len) => {
                    out.push_str(&p[i..i + len]);
                    for _ in 1..len {
                        chars.next();
                    }
                }
                None => out.push_str(r"\{"),
            },
            '}' if !in_class => out.push_str(r"\}"),
            _ => out.push(c),
        }
    }
    out
}

/// The length of the `{n}`, `{n,}` or `{n,m}` quantifier `s` starts with.
fn quantifier_len(s: &str) -> Option<usize> {
    let body = &s[1..];
    let digits = body.len() - body.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return None;
    }
    let rest = &body[digits..];
    let tail = if let Some(after) = rest.strip_prefix(',') {
        1 + after.len() - after.trim_start_matches(|c: char| c.is_ascii_digit()).len()
    } else {
        0
    };
    rest[tail..]
        .starts_with('}')
        .then_some(1 + digits + tail + 1)
}

/// `GetPathToCheck`: `/` separators (Windows only, as upstream), and a
/// trailing `/` on a folder so `name/` lines match it.
fn path_to_check(path: &str, is_dir: bool, normalize: bool) -> String {
    let mut p = if normalize {
        path.replace('\\', "/")
    } else {
        path.to_owned()
    };
    if is_dir {
        p.truncate(p.trim_end_matches('/').len());
        p.push('/');
    }
    p
}

/// A `.ignore` file's rules (`None`: everything), with the length and
/// modification time they were read at.
type ParsedFile = (u64, Option<SystemTime>, Option<IgnoreRules>);

/// `DotIgnoreIgnoreRule` and its two caches. One lives for one scan (whose
/// start clears upstream's directory cache, `ClearIgnoreRuleCache`) or one
/// reported change, so neither cache needs a bound.
#[derive(Default)]
pub(crate) struct DotIgnoreRule {
    /// Folder → the nearest folder at or above it holding a `.ignore`.
    dirs: HashMap<String, Option<String>>,
    /// `.ignore` path → its rules, re-read when the file changes.
    files: HashMap<String, ParsedFile>,
}

impl DotIgnoreRule {
    /// `ShouldIgnore`: whether the nearest `.ignore` at or above `path`'s
    /// folder (the folder itself, for a folder) hides `path`.
    pub(crate) fn should_ignore(&mut self, path: &str, is_dir: bool) -> bool {
        let search = if is_dir {
            Some(path)
        } else {
            Path::new(path).parent().and_then(Path::to_str)
        };
        let Some(search) = search.filter(|s| !s.is_empty()) else {
            return false;
        };
        let Some(dir) = self.find_ignore_dir(search) else {
            return false;
        };
        let file = Path::new(&dir).join(".ignore");
        let Some((_, _, rules)) = self.parsed(&file) else {
            // Deleted since the folder was cached.
            self.dirs.remove(search);
            return false;
        };
        rules
            .as_ref()
            .is_none_or(|rules| rules.is_ignored(&path_to_check(path, is_dir, cfg!(windows))))
    }

    /// `FindIgnoreFileCached`: walks up from `directory` to the first folder
    /// holding a `.ignore`, caching the answer for every folder it passed.
    fn find_ignore_dir(&mut self, directory: &str) -> Option<String> {
        if let Some(cached) = self.dirs.get(directory) {
            return cached.clone();
        }
        let mut checked = vec![directory.to_owned()];
        let mut found = None;
        for current in Path::new(directory).ancestors() {
            let Some(current) = current.to_str().filter(|c| !c.is_empty()) else {
                break;
            };
            if current != directory
                && let Some(cached) = self.dirs.get(current)
            {
                found.clone_from(cached);
                break;
            }
            let is_file = std::fs::symlink_metadata(Path::new(current).join(".ignore"))
                .is_ok_and(|meta| !meta.is_dir());
            if is_file {
                found = Some(current.to_owned());
                break;
            }
            if current != directory {
                checked.push(current.to_owned());
            }
        }
        for dir in checked {
            self.dirs.insert(dir, found.clone());
        }
        found
    }

    /// `GetParsedRules`: `file`'s rules, read again when its length or
    /// modification time changed; `None` once it is gone.
    fn parsed(&mut self, file: &Path) -> Option<&ParsedFile> {
        let key = file.to_string_lossy().into_owned();
        let meta = match std::fs::metadata(file) {
            Ok(meta) => meta,
            // A link whose target is gone counts as an empty file.
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && std::fs::symlink_metadata(file).is_ok() =>
            {
                self.files.insert(key.clone(), (0, None, None));
                return self.files.get(&key);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.files.remove(&key);
                return None;
            }
            // Unreachable (a stale mount, a permission): upstream would
            // hide everything, which would prune the folder's items on a
            // passing fault. It hides nothing until it can be read.
            Err(error) => {
                let unreadable = (0, None, Some(IgnoreRules(Vec::new())));
                if !self.files.get(&key).is_some_and(|(len, modified, rules)| {
                    *len == 0
                        && modified.is_none()
                        && rules.as_ref().is_some_and(|r| r.0.is_empty())
                }) {
                    tracing::warn!(path = %file.display(), %error, ".ignore file could not be read; it hides nothing");
                }
                self.files.insert(key.clone(), unreadable);
                return self.files.get(&key);
            }
        };
        let (len, modified) = (meta.len(), meta.modified().ok());
        if !self
            .files
            .get(&key)
            .is_some_and(|(l, m, _)| *l == len && *m == modified)
        {
            let rules = read_rules(file, len);
            self.files.insert(key.clone(), (len, modified, rules));
        }
        self.files.get(&key)
    }
}

/// `ParseIgnoreFile`: `None` (everything) for an empty or blank file or one
/// without a valid line.
fn read_rules(file: &Path, len: u64) -> Option<IgnoreRules> {
    if len == 0 {
        return None;
    }
    let content = match std::fs::read(file) {
        // `File.ReadAllText`: BOM-aware (UTF-8, UTF-16, UTF-32) and lossy.
        Ok(bytes) => ferrofin_util::stream_extensions::decode_stream_text(bytes),
        Err(error) => {
            // Upstream's read throws out of the resolver; hiding nothing
            // keeps the folder's items rather than pruning them.
            tracing::warn!(path = %file.display(), %error, ".ignore file could not be read; it hides nothing");
            return Some(IgnoreRules(Vec::new()));
        }
    };
    IgnoreRules::parse(
        content
            .split('\n')
            .map(str::trim)
            .filter(|line| !line.is_empty()),
        file,
    )
}

#[cfg(test)]
mod tests {
    use super::{DotIgnoreRule, IgnoreRules, path_to_check};

    const RULE1: &[&str] = &["SPs"];
    const RULE2: &[&str] = &["SPs", "!thebestshot.mkv"];
    const RULE3: &[&str] = &[
        "*.txt",
        r"{\colortbl;\red255\green255\blue255;}",
        "videos/",
        r"\invalid\escape\sequence",
        "*.mkv",
    ];
    const RULE4: &[&str] = &[
        r"{\colortbl;\red255\green255\blue255;}",
        r"\invalid\escape\sequence",
    ];

    /// `DotIgnoreIgnoreRule.CheckIgnoreRules`.
    fn check(path: &str, rules: &[&str], is_dir: bool, normalize: bool) -> bool {
        IgnoreRules::parse(rules.iter().copied(), std::path::Path::new(".ignore"))
            .is_none_or(|rules| rules.is_ignored(&path_to_check(path, is_dir, normalize)))
    }

    /// `CheckIgnoreRulesTestData` and `WindowsPathNormalizationTestData`
    /// (`DotIgnoreIgnoreRuleTest.cs`), plus a braces line .NET reads
    /// literally.
    #[rstest::rstest]
    #[case(RULE1, "f:/cd/sps/ffffff.mkv", false, true)]
    #[case(RULE1, "cd/sps/ffffff.mkv", false, true)]
    #[case(RULE1, "/cd/sps/ffffff.mkv", false, true)]
    #[case(RULE2, "f:/cd/sps/ffffff.mkv", false, true)]
    #[case(RULE2, "cd/sps/ffffff.mkv", false, true)]
    #[case(RULE2, "/cd/sps/ffffff.mkv", false, true)]
    #[case(RULE2, "f:/cd/sps/thebestshot.mkv", false, false)]
    #[case(RULE2, "cd/sps/thebestshot.mkv", false, false)]
    #[case(RULE2, "/cd/sps/thebestshot.mkv", false, false)]
    #[case(RULE3, "test.txt", false, true)]
    #[case(RULE3, "videos/movie.mp4", false, true)]
    #[case(RULE3, "movie.mkv", false, true)]
    #[case(RULE3, "test.mp3", false, false)]
    #[case(RULE4, "any-file.txt", false, true)]
    #[case(RULE4, "any/path/to/file.mkv", false, true)]
    #[case(&["*{tmdb-603}*"], "/m/The Matrix {tmdb-603}/The Matrix.mkv", false, true)]
    #[case(&["*{tmdb-603}*"], "/m/Heat {tmdb-949}/Heat.mkv", false, false)]
    fn check_ignore_rules_returns_expected_result(
        #[case] rules: &[&str],
        #[case] path: &str,
        #[case] is_dir: bool,
        #[case] expected: bool,
    ) {
        assert_eq!(check(path, rules, is_dir, false), expected, "{path}");
    }

    #[rstest::rstest]
    #[case(RULE1, r"C:\cd\sps\ffffff.mkv", false, true)]
    #[case(RULE1, r"D:\media\sps\movie.mkv", false, true)]
    #[case(RULE1, r"\\server\share\sps\file.mkv", false, true)]
    #[case(RULE2, r"C:\cd\sps\ffffff.mkv", false, true)]
    #[case(RULE2, r"C:\cd\sps\thebestshot.mkv", false, false)]
    #[case(RULE3, r"C:\videos\movie.mp4", false, true)]
    #[case(RULE3, r"D:\documents\test.txt", false, true)]
    #[case(RULE3, r"E:\music\song.mp3", false, false)]
    fn check_ignore_rules_with_windows_paths_normalizes_backslashes(
        #[case] rules: &[&str],
        #[case] path: &str,
        #[case] is_dir: bool,
        #[case] expected: bool,
    ) {
        assert_eq!(check(path, rules, is_dir, true), expected, "{path}");
    }

    #[rstest::rstest]
    #[case(r"C:\cd\sps\ffffff.mkv")]
    #[case(r"D:\media\sps\movie.mkv")]
    fn check_ignore_rules_with_windows_paths_without_normalization_does_not_match(
        #[case] path: &str,
    ) {
        assert!(!check(path, RULE1, false, false));
    }

    fn ignored(rule: &mut DotIgnoreRule, path: &std::path::Path, is_dir: bool) -> bool {
        rule.should_ignore(path.to_str().expect("utf-8 path"), is_dir)
    }

    #[test]
    fn cache_hit_repeated_calls_do_not_reread_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sub = tmp.path().join("subdir");
        std::fs::create_dir(&sub).expect("mkdir");
        std::fs::write(tmp.path().join(".ignore"), "*.tmp").expect("write");
        let mut rule = DotIgnoreRule::default();
        assert!(ignored(&mut rule, &sub.join("test.tmp"), false));
        assert!(ignored(&mut rule, &sub.join("test.tmp"), false));
        assert!(ignored(&mut rule, &sub.join("other.tmp"), false));
        assert!(!ignored(&mut rule, &sub.join("other.txt"), false));
    }

    #[test]
    fn cache_invalidation_modify_ignore_file_reparses() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join(".ignore");
        std::fs::write(&file, "*.tmp").expect("write");
        let mut rule = DotIgnoreRule::default();
        assert!(ignored(&mut rule, &tmp.path().join("test.tmp"), false));
        std::thread::sleep(std::time::Duration::from_millis(50));
        // A different length, so the change shows on any mtime resolution.
        std::fs::write(&file, "*.txt\n").expect("write");
        assert!(!ignored(&mut rule, &tmp.path().join("test.tmp"), false));
        assert!(ignored(&mut rule, &tmp.path().join("test.txt"), false));
    }

    #[rstest::rstest]
    #[case::empty("")]
    #[case::whitespace_only("   \n\t\n   ")]
    #[case::comments_only_parse_but_match_nothing("# notes")]
    fn an_empty_ignore_file_ignores_everything(#[case] content: &str) {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join(".ignore"), content).expect("write");
        let mut rule = DotIgnoreRule::default();
        let expected = !content.starts_with('#');
        assert_eq!(
            ignored(&mut rule, &tmp.path().join("anyfile.mkv"), false),
            expected
        );
    }

    #[test]
    fn no_ignore_file_does_not_ignore() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut rule = DotIgnoreRule::default();
        assert!(!ignored(&mut rule, &tmp.path().join("anyfile.mkv"), false));
    }

    #[test]
    fn ignore_file_deleted_handles_gracefully() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join(".ignore");
        std::fs::write(&file, "*.tmp").expect("write");
        let mut rule = DotIgnoreRule::default();
        assert!(ignored(&mut rule, &tmp.path().join("test.tmp"), false));
        std::fs::remove_file(&file).expect("delete");
        assert!(!ignored(&mut rule, &tmp.path().join("test.tmp"), false));
    }

    #[test]
    fn parent_directory_ignore_file_applies_to_subdirectories() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sub2 = tmp.path().join("sub1/sub2");
        std::fs::create_dir_all(&sub2).expect("mkdir");
        std::fs::write(tmp.path().join(".ignore"), "*.tmp").expect("write");
        let mut rule = DotIgnoreRule::default();
        assert!(ignored(&mut rule, &sub2.join("test.tmp"), false));
        assert!(ignored(&mut rule, &tmp.path().join("sub1/test.tmp"), false));
    }

    #[test]
    fn directory_matching_trailing_slash_pattern() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let videos = tmp.path().join("videos");
        std::fs::create_dir(&videos).expect("mkdir");
        std::fs::write(tmp.path().join(".ignore"), "videos/").expect("write");
        let mut rule = DotIgnoreRule::default();
        assert!(ignored(&mut rule, &videos, true));
        // A file named `videos` is not a folder: `(/|^)videos/` needs the slash.
        assert!(!ignored(&mut rule, &tmp.path().join("videos"), false));
    }

    /// A folder's own `.ignore` hides the folder itself, and the nearest
    /// `.ignore` wins over one further up.
    #[test]
    fn the_nearest_ignore_file_decides() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let private = tmp.path().join("Private");
        std::fs::create_dir(&private).expect("mkdir");
        std::fs::write(private.join(".ignore"), "").expect("write");
        std::fs::write(tmp.path().join(".ignore"), "*.part").expect("write");
        let mut rule = DotIgnoreRule::default();
        assert!(ignored(&mut rule, &private, true));
        assert!(ignored(&mut rule, &private.join("a.mkv"), false));
        assert!(ignored(&mut rule, &tmp.path().join("a.mkv.part"), false));
        assert!(!ignored(&mut rule, &tmp.path().join("a.mkv"), false));
    }

    /// A `.ignore` saved with a byte-order mark keeps its first line.
    #[rstest::rstest]
    #[case::utf8(b"\xEF\xBB\xBFdrafts/\n".to_vec())]
    #[case::utf16_le(b"\xFF\xFEd\0r\0a\0f\0t\0s\0/\0".to_vec())]
    fn a_byte_order_mark_is_not_part_of_the_first_line(#[case] content: Vec<u8>) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let drafts = tmp.path().join("drafts");
        std::fs::create_dir(&drafts).expect("mkdir");
        std::fs::write(tmp.path().join(".ignore"), content).expect("write");
        let mut rule = DotIgnoreRule::default();
        assert!(ignored(&mut rule, &drafts, true));
        assert!(!ignored(&mut rule, &tmp.path().join("a.mkv"), false));
    }

    /// A `.ignore` link whose target is gone hides everything, as an empty
    /// file does (`ParseIgnoreFile`).
    #[cfg(unix)]
    #[test]
    fn a_dangling_ignore_link_ignores_everything() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::os::unix::fs::symlink(tmp.path().join("gone"), tmp.path().join(".ignore"))
            .expect("symlink");
        let mut rule = DotIgnoreRule::default();
        assert!(ignored(&mut rule, &tmp.path().join("a.mkv"), false));
    }
}
