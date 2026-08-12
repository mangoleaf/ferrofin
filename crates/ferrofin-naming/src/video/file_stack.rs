//! Port of `Emby.Naming.Video.FileStack`.

/// A list of file paths with additional stacking information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStack {
    /// The name of the file stack.
    pub name: String,
    /// The list of paths in the stack.
    pub files: Vec<String>,
    /// Whether this stack is a directory stack.
    pub is_directory_stack: bool,
}

impl FileStack {
    /// Creates a new [`FileStack`].
    #[must_use]
    pub fn new(name: impl Into<String>, is_directory: bool, files: Vec<String>) -> Self {
        Self {
            name: name.into(),
            files,
            is_directory_stack: is_directory,
        }
    }

    /// Determines whether the given `file` is in this stack.
    #[must_use]
    pub fn contains_file(&self, file: &str, is_directory: bool) -> bool {
        if file.is_empty() {
            return false;
        }

        self.is_directory_stack == is_directory
            && self.files.iter().any(|f| f.eq_ignore_ascii_case(file))
    }
}
