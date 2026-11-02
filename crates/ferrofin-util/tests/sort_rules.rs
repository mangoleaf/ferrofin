//! The installed sort rules are process-wide, so the one test that changes
//! them runs in a test binary of its own.

use ferrofin_util::sort_name::{
    SortRules, create_sort_name, forced_sort_key, install_sort_rules, previous_create_sort_name,
};

/// Every derivation reads the rules installed now (`GetSortName` reads
/// `ConfigurationManager.Configuration` on each call): the server's lists
/// once installed, and the defaults again once those are reinstalled. The
/// migration-only `previous_*` key stays on the defaults throughout.
#[test]
fn derivations_follow_the_installed_rules() {
    assert_eq!(create_sort_name("The Matrix"), "matrix");

    install_sort_rules(SortRules {
        remove_words: vec!["der".to_owned(), "die".to_owned(), "das".to_owned()],
        remove_characters: Vec::new(),
        replace_characters: Vec::new(),
    });
    assert_eq!(create_sort_name("Der Untergang"), "untergang");
    assert_eq!(create_sort_name("The Matrix"), "the matrix");
    assert_eq!(forced_sort_key("Das Boot"), "boot");
    assert_eq!(previous_create_sort_name("The Matrix"), "matrix");

    install_sort_rules(SortRules::default());
    assert_eq!(create_sort_name("The Matrix"), "matrix");
}
