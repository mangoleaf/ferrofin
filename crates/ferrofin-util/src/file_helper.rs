//! Port of `FileHelper.cs` — create-or-truncate a file at a path.

use std::io;
use std::path::Path;

/// Creates, or truncates, a file at the specified path.
///
/// # Errors
///
/// Returns any I/O error raised while creating/truncating the file.
pub fn create_empty<P: AsRef<Path>>(path: P) -> io::Result<()> {
    std::fs::File::create(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::temp_dir;

    #[test]
    fn create_empty_valid_correct() {
        let path = temp_dir().join(format!("ferrofin-{}.tmp", uuid::Uuid::new_v4()));
        assert!(!path.exists());

        create_empty(&path).expect("create should succeed");
        assert!(path.exists());

        std::fs::remove_file(&path).expect("cleanup");
    }
}
