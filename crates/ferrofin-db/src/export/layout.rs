//! Where an export reads the stopped server's directories, and the layout
//! Jellyfin reads the output in (gl-1557, decision D5).
//!
//! The output is Jellyfin's program data directory in the layout of its
//! docker image: one `/config` volume, with `config`, `data`, `metadata`,
//! `plugins` and `root` under it. The data directory is copied to it whole,
//! and a configuration or metadata directory the server kept elsewhere is
//! copied to the place Jellyfin reads it, `config` or `metadata`. The paths the
//! database stores are rewritten onto the same layout (`super::paths`).
//!
//! The layout is a value ([`Layout`]): another one (a Debian package's, or
//! directories the operator names) is another value, not literals to find.

use std::path::{Path, PathBuf};

use super::failed;
use crate::{DbError, Result};

/// Where Jellyfin reads an export: the directories the stored paths name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// Its program data directory (`JELLYFIN_DATA_DIR`), which holds `data`,
    /// `metadata`, `plugins` and `root`: the export's output.
    pub program_data: &'static str,
    /// Its configuration directory (`JELLYFIN_CONFIG_DIR`).
    pub config: &'static str,
}

impl Layout {
    /// Its metadata directory: the default, `{program data}/metadata`, as the
    /// exported `system.xml` leaves `MetadataPath` empty.
    #[must_use]
    pub fn metadata(&self) -> String {
        format!("{}/{METADATA}", self.program_data)
    }

    /// Where `dir`, one of its directories, is in the output: its place under
    /// program data. A directory outside program data has none (the export
    /// writes one directory).
    fn place(&self, dir: &str) -> Result<PathBuf> {
        Path::new(dir)
            .strip_prefix(self.program_data)
            .map(Path::to_owned)
            .map_err(|_| DbError::ExportRefused {
                reason: format!(
                    "the layout keeps {dir} outside its program data directory {}, which the \
                     export does not write",
                    self.program_data
                ),
            })
    }
}

/// The layout of Jellyfin's docker image (`JELLYFIN_DATA_DIR=/config`,
/// `JELLYFIN_CONFIG_DIR=/config/config`), which every export writes (D5).
pub const DOCKER: Layout = Layout {
    program_data: "/config",
    config: "/config/config",
};

/// The configuration directory's place under program data in the source's
/// default layout, which is Jellyfin's too.
pub(super) const CONFIG: &str = "config";

/// The metadata directory's place under program data, likewise.
pub(super) const METADATA: &str = "metadata";

/// The directories of the stopped server an export reads, as the export
/// reaches them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDirs {
    /// Its data directory: Jellyfin's program data, copied to the output.
    pub program_data: PathBuf,
    /// Its configuration directory, copied to the layout's configuration
    /// directory when it is not the data directory's own.
    pub config: PathBuf,
    /// Its metadata directory, as its settings name it, copied to the layout's
    /// metadata directory when it is not the data directory's own.
    pub metadata: PathBuf,
    /// Its directories that hold nothing Jellyfin reads (its cache, its log,
    /// its web client): left out where they are inside the data directory.
    pub not_needed: Vec<PathBuf>,
    /// Its files that Jellyfin does not read and the export either writes for
    /// it in another form (its configuration, written as XML) or has no use
    /// for (its tasks' last results), each of which may or may not exist:
    /// digested, never copied, wherever they are. A copy of the configuration
    /// would outrank that XML at a later adoption by Ferrofin.
    pub not_needed_files: Vec<PathBuf>,
}

/// Why a directory inside the data directory was left out of its copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeftOut {
    /// The data directory's own configuration or metadata directory, while
    /// the server used one kept elsewhere, which the output holds instead.
    NotUsed,
    /// The server's configuration or metadata directory, inside the data
    /// directory at another place than Jellyfin's: copied to Jellyfin's.
    Moved,
    /// A directory of the server that holds nothing Jellyfin reads: its
    /// cache, log or web client.
    NotNeeded,
}

impl SourceDirs {
    /// A data directory with its configuration and metadata in their default
    /// places under it, and nothing to leave out.
    #[must_use]
    pub fn new(program_data: &Path) -> Self {
        Self {
            program_data: program_data.to_owned(),
            config: program_data.join(CONFIG),
            metadata: program_data.join(METADATA),
            not_needed: Vec::new(),
            not_needed_files: Vec::new(),
        }
    }

    /// What the copy into `layout` reads: `(source directory, its place in the
    /// output)`, the data directory first, and what it leaves out of the data
    /// directory's walk, relative to it (only those that exist).
    ///
    /// A configuration or metadata directory kept elsewhere is its own tree,
    /// and the data directory's own `config` or `metadata` is left out: Jellyfin
    /// reads one directory there, and it must be the one the server used.
    pub(super) fn trees(&self, layout: &Layout) -> Result<Trees> {
        let mut trees = Trees {
            copied: vec![(self.program_data.clone(), PathBuf::new())],
            left_out: Vec::new(),
        };
        for (dir, own, to) in [
            (&self.config, CONFIG, layout.config.to_owned()),
            (&self.metadata, METADATA, layout.metadata()),
        ] {
            let own = self.program_data.join(own);
            if same_dir(dir, &own)? {
                continue;
            }
            if exists(dir)? {
                trees.copied.push((dir.clone(), layout.place(&to)?));
            }
            trees.leave_out(&self.program_data, &own, LeftOut::NotUsed)?;
            trees.leave_out(&self.program_data, dir, LeftOut::Moved)?;
        }
        for dir in &self.not_needed {
            trees.leave_out(&self.program_data, dir, LeftOut::NotNeeded)?;
        }
        Ok(trees)
    }
}

/// What an export copies ([`SourceDirs::trees`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Trees {
    /// `(source directory, its place relative to the output)`.
    pub(super) copied: Vec<(PathBuf, PathBuf)>,
    /// Directories inside the data directory left out of its copy, relative
    /// to it, with why.
    pub(super) left_out: Vec<(PathBuf, LeftOut)>,
}

impl Trees {
    /// The directories left out, relative to the data directory.
    pub(super) fn left_out_dirs(&self) -> Vec<PathBuf> {
        self.left_out.iter().map(|(dir, _)| dir.clone()).collect()
    }

    /// Leaves `dir` out of the copy of `program_data`, for `why`, when it
    /// exists and is inside it (not the data directory itself): by its path as
    /// given (the walk matches names, so a link there is left out whole) and
    /// by its real path.
    fn leave_out(&mut self, program_data: &Path, dir: &Path, why: LeftOut) -> Result<()> {
        if !exists(dir)? {
            return Ok(());
        }
        let real_dir = real(dir)?;
        let real_root = real(program_data)?;
        for relative in [
            dir.strip_prefix(program_data).ok(),
            real_dir.strip_prefix(&real_root).ok(),
        ]
        .into_iter()
        .flatten()
        {
            if !relative.as_os_str().is_empty()
                && !self.left_out.iter().any(|(kept, _)| kept == relative)
            {
                self.left_out.push((relative.to_owned(), why));
            }
        }
        Ok(())
    }
}

/// Whether `a` and `b` are one directory: the same path, or the same real
/// path when both exist.
fn same_dir(a: &Path, b: &Path) -> Result<bool> {
    if a == b {
        return Ok(true);
    }
    Ok(exists(a)? && exists(b)? && real(a)? == real(b)?)
}

fn exists(path: &Path) -> Result<bool> {
    path.try_exists().map_err(failed(path))
}

fn real(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path).map_err(failed(path))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{DOCKER, Layout, LeftOut, SourceDirs};

    #[test]
    fn the_docker_layout_keeps_everything_under_config() {
        assert_eq!(DOCKER.program_data, "/config");
        assert_eq!(DOCKER.config, "/config/config");
        assert_eq!(DOCKER.metadata(), "/config/metadata");
    }

    #[test]
    fn a_default_data_directory_is_one_tree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::create_dir_all(dir.path().join("metadata")).unwrap();

        let trees = SourceDirs::new(dir.path()).trees(&DOCKER).unwrap();

        assert_eq!(trees.copied, [(dir.path().to_owned(), PathBuf::new())]);
        assert_eq!(trees.left_out.len(), 0);
    }

    /// A metadata directory outside the data directory is copied to the
    /// output's `metadata`, and the data directory's own is left out; one
    /// inside it, elsewhere than `metadata`, is left out of the data
    /// directory's copy so it is not copied twice. What does not exist is
    /// neither copied nor left out.
    #[test]
    fn directories_kept_elsewhere_are_trees_of_their_own() {
        let scratch = tempfile::tempdir().unwrap();
        let data = scratch.path().join("data");
        let elsewhere = scratch.path().join("meta");
        for made in ["data/metadata", "data/cache", "data/etc", "meta"] {
            std::fs::create_dir_all(scratch.path().join(made)).unwrap();
        }
        let dirs = SourceDirs {
            config: data.join("etc"),
            metadata: elsewhere.clone(),
            not_needed: vec![
                data.join("cache"),
                data.join("log"),
                scratch.path().join("web"),
            ],
            ..SourceDirs::new(&data)
        };

        let trees = dirs.trees(&DOCKER).unwrap();

        assert_eq!(
            trees.copied,
            [
                (data.clone(), PathBuf::new()),
                (data.join("etc"), PathBuf::from("config")),
                (elsewhere, PathBuf::from("metadata")),
            ]
        );
        assert_eq!(
            trees.left_out,
            [
                (PathBuf::from("etc"), LeftOut::Moved),
                (PathBuf::from("metadata"), LeftOut::NotUsed),
                (PathBuf::from("cache"), LeftOut::NotNeeded),
            ]
        );
    }

    /// A not-needed directory that is a link to another volume is left out by
    /// the name the walk finds it under, and a layout that keeps a directory
    /// outside its program data cannot place it.
    #[cfg(unix)]
    #[test]
    fn a_linked_directory_is_left_out_by_its_name() {
        let scratch = tempfile::tempdir().unwrap();
        let data = scratch.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(scratch.path().join("volume")).unwrap();
        std::os::unix::fs::symlink(scratch.path().join("volume"), data.join("cache")).unwrap();
        let dirs = SourceDirs {
            not_needed: vec![data.join("cache")],
            ..SourceDirs::new(&data)
        };

        let trees = dirs.trees(&DOCKER).unwrap();

        assert_eq!(
            trees.left_out,
            [(PathBuf::from("cache"), LeftOut::NotNeeded)]
        );
        let apart = Layout {
            program_data: "/var/lib/jellyfin",
            config: "/etc/jellyfin",
        };
        let error = SourceDirs {
            config: scratch.path().join("volume"),
            ..SourceDirs::new(&data)
        }
        .trees(&apart)
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("outside its program data directory"),
            "{error}"
        );
    }
}
