//! Rebuild when the migrations directory changes.
//!
//! `sqlx::migrate!` embeds the directory at compile time but only watches the
//! files that existed at the last expansion — ADDING a migration would
//! otherwise ship stale embedded migrations until an unrelated rebuild.

fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
