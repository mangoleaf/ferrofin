//! Explicit, query-time .NET invariant casing for SQLite.
//!
//! Never override SQLite's built-ins: indexes written by other SQLite builds
//! must keep their original meaning. These functions are DIRECTONLY so they
//! cannot become persistent expression-index, trigger, or view dependencies.

use std::ffi::{c_char, c_int};

use ferrofin_util::string_extensions::{get_clean_value, lower_invariant, upper_invariant};
use libsqlite3_sys as ffi;

type Mapping = fn(&str) -> String;

unsafe extern "C" fn map_text(
    ctx: *mut ffi::sqlite3_context,
    _argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
) {
    // SAFETY: registered with arity one. SQLite owns the argument and context
    // throughout the callback. The mapping pointer points to a static fn pointer.
    unsafe {
        let value = *argv;
        if ffi::sqlite3_value_type(value) == ffi::SQLITE_NULL {
            ffi::sqlite3_result_null(ctx);
            return;
        }
        let ptr = ffi::sqlite3_value_text(value);
        if ptr.is_null() {
            ffi::sqlite3_result_error_nomem(ctx);
            return;
        }
        let len = ffi::sqlite3_value_bytes(value);
        let Ok(len) = usize::try_from(len) else {
            ffi::sqlite3_result_error(ctx, c"invalid text length".as_ptr(), -1);
            return;
        };
        let bytes = std::slice::from_raw_parts(ptr, len);
        let Ok(input) = std::str::from_utf8(bytes) else {
            ffi::sqlite3_result_error(ctx, c"invalid UTF-8 in casing input".as_ptr(), -1);
            return;
        };
        let mapping = *ffi::sqlite3_user_data(ctx).cast::<Mapping>();
        // Rust must never unwind across SQLite's C stack.
        let Ok(output) = std::panic::catch_unwind(|| mapping(input)) else {
            ffi::sqlite3_result_error(ctx, c"Unicode casing failed".as_ptr(), -1);
            return;
        };
        let Ok(len) = c_int::try_from(output.len()) else {
            ffi::sqlite3_result_error_toobig(ctx);
            return;
        };
        // TRANSIENT copies the explicit-length buffer, including embedded NULs,
        // before the Rust string is dropped.
        ffi::sqlite3_result_text(ctx, output.as_ptr().cast(), len, ffi::SQLITE_TRANSIENT());
    }
}

unsafe extern "C" fn register_on_connection(
    db: *mut ffi::sqlite3,
    _err_msg: *mut *mut c_char,
    _api: *const ffi::sqlite3_api_routines,
) -> c_int {
    // Fixed on Jellyfin's default sort rules, not the configured ones: only
    // released migrations call these, and they are registered deterministic.
    static SORT: Mapping = ferrofin_util::sort_name::default_rules_sort_name;
    static PREVIOUS_SORT: Mapping = ferrofin_util::sort_name::previous_create_sort_name;
    static FORCED_SORT: Mapping = ferrofin_util::sort_name::default_rules_forced_sort_key;
    static PREVIOUS_FORCED_SORT: Mapping = ferrofin_util::sort_name::previous_forced_sort_key;
    static LOWER: Mapping = lower_invariant;
    static UPPER: Mapping = upper_invariant;
    static CLEAN: Mapping = get_clean_value;
    static PREVIOUS_CLEAN: Mapping = |value| {
        if value.trim().is_empty() {
            value.to_owned()
        } else {
            ferrofin_util::string_extensions::remove_diacritics(value).to_lowercase()
        }
    };
    for (name, mapping) in [
        (c"ferrofin_sort_name", &SORT),
        (c"ferrofin_previous_sort_name", &PREVIOUS_SORT),
        (c"ferrofin_forced_sort_key", &FORCED_SORT),
        (c"ferrofin_previous_forced_sort_key", &PREVIOUS_FORCED_SORT),
        (c"ferrofin_lower_invariant", &LOWER),
        (c"ferrofin_upper_invariant", &UPPER),
        (c"ferrofin_clean_value", &CLEAN),
        (c"ferrofin_previous_clean_value", &PREVIOUS_CLEAN),
    ] {
        // SAFETY: SQLite supplies a live connection. Names and mappings are
        // static, and map_text has the required callback signature.
        let rc = unsafe {
            ffi::sqlite3_create_function_v2(
                db,
                name.as_ptr(),
                1,
                ffi::SQLITE_UTF8 | ffi::SQLITE_DETERMINISTIC | ffi::SQLITE_DIRECTONLY,
                std::ptr::from_ref(mapping).cast_mut().cast(),
                Some(map_text),
                None,
                None,
                None,
            )
        };
        if rc != ffi::SQLITE_OK {
            return rc;
        }
    }
    ffi::SQLITE_OK
}

pub(crate) fn register() {
    // SAFETY: the entry point is static and sqlite3_auto_extension is thread safe.
    let rc = unsafe { ffi::sqlite3_auto_extension(Some(register_on_connection)) };
    assert_eq!(
        rc,
        ffi::SQLITE_OK,
        "register SQLite Unicode casing functions"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rstest::rstest]
    #[case("Élodie")]
    #[case("ΟΣ")]
    #[case("ςσΣ")]
    #[case("ıİiI")]
    #[case("ᾀᾈ𐐨𐐀")]
    #[case("Straße")]
    #[case("A\0É")]
    #[case("")]
    #[tokio::test]
    async fn sql_matches_rust(#[case] input: &str) {
        let db = crate::Database::connect_in_memory().await.unwrap();
        let result: (String, String, String) = sqlx::query_as(
            "SELECT ferrofin_lower_invariant(?1), ferrofin_upper_invariant(?1), ferrofin_clean_value(?1)",
        ).bind(input).fetch_one(db.pool()).await.unwrap();
        assert_eq!(
            result,
            (
                lower_invariant(input),
                upper_invariant(input),
                get_clean_value(input)
            )
        );
        let null: Option<String> = sqlx::query_scalar("SELECT ferrofin_upper_invariant(NULL)")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(null, None);
    }

    #[tokio::test]
    async fn builtins_and_persistent_schema_keep_their_original_semantics() {
        let db = crate::Database::connect_in_memory().await.unwrap();
        let value: String = sqlx::query_scalar("SELECT lower('É')")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(value, "É", "do not override SQLite lower");
        sqlx::query("CREATE TABLE casing_test (name TEXT)")
            .execute(db.writer())
            .await
            .unwrap();
        assert!(
            sqlx::query("CREATE INDEX unsafe_index ON casing_test(ferrofin_upper_invariant(name))")
                .execute(db.writer())
                .await
                .is_err()
        );
        assert!(
            sqlx::query("SELECT ferrofin_lower_invariant(CAST(x'ff' AS TEXT))")
                .fetch_one(db.pool())
                .await
                .is_err(),
            "invalid UTF-8 must not panic or be silently replaced"
        );
    }

    #[tokio::test]
    async fn functions_reach_writer_reader_and_reopened_pools() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("case.db").display());
        for _ in 0..2 {
            let db = crate::Database::connect(&url).await.unwrap();
            for pool in [db.pool(), db.writer()] {
                let value: String = sqlx::query_scalar("SELECT ferrofin_upper_invariant('é')")
                    .fetch_one(pool)
                    .await
                    .unwrap();
                assert_eq!(value, "É");
            }
        }
    }
}
