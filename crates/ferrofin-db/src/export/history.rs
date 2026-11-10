//! The `__EFMigrationsHistory` an export records: exactly the schema migrations
//! the database already has for its source generation, and none of Jellyfin's
//! code migrations, which Jellyfin then runs itself. The EF history rewrite
//! (gl-1549) writes it here.
