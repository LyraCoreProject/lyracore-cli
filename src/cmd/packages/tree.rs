//! One traversal policy for Package trees.
//!
//! Package review, copying and provenance must see the same entries. In particular, none may
//! follow a symlink outside the reviewed folder, silently skip an unreadable entry, or disagree
//! about whether an empty directory is content.

use crate::{Error, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Directory,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: PathBuf,
    pub relative: PathBuf,
    pub kind: EntryKind,
}

/// Every ordinary file and directory below `root`, sorted by relative path.
///
/// Nested `.git` entries are excluded because `packages add` intentionally does not install a
/// source repository's own metadata. Symlinks and special files are refused before consent: a
/// symlink would make the reviewed bytes depend on an external target, while a FIFO/socket/device
/// cannot be copied as an inert Package file. Non-UTF-8 names are also refused because the
/// persisted identity is portable text and must name every entry without lossy collisions.
pub fn collect(root: &Path) -> Result<Vec<TreeEntry>> {
    let mut entries = Vec::new();
    collect_from(root, root, &mut entries)?;
    entries.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(entries)
}

fn collect_from(root: &Path, dir: &Path, entries: &mut Vec<TreeEntry>) -> Result<()> {
    for result in std::fs::read_dir(dir)? {
        let entry = result?;
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        let relative = path.strip_prefix(root).map_err(|_| {
            Error::State(format!(
                "Package entry {} escaped its tree root {}",
                path.display(),
                root.display()
            ))
        })?;
        if relative.to_str().is_none() {
            return Err(Error::Usage(format!(
                "{} has a file name that is not valid UTF-8. Package identities are persisted as \
                 portable text, so every file and directory name must be valid UTF-8. Nothing \
                 was copied.",
                path.display()
            )));
        }

        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(Error::Usage(format!(
                "{} is a symlink. A Package is copied, never linked, so every entry must be an \
                 ordinary file or directory held inside the reviewed folder. Nothing was copied.",
                path.display()
            )));
        }
        let kind = if file_type.is_dir() {
            EntryKind::Directory
        } else if file_type.is_file() {
            EntryKind::File
        } else {
            return Err(Error::Usage(format!(
                "{} is a special filesystem entry. Packages may contain only ordinary files and \
                 directories. Nothing was copied.",
                path.display()
            )));
        };
        entries.push(TreeEntry {
            path: path.clone(),
            relative: relative.to_path_buf(),
            kind,
        });
        if kind == EntryKind::Directory {
            collect_from(root, &path, entries)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn a_nested_symlink_is_refused_instead_of_followed() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::os::unix::fs::symlink("/", tmp.path().join("src/outside")).unwrap();

        let error = collect(tmp.path()).unwrap_err();

        assert!(error.to_string().contains("symlink"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_name_is_refused_without_lossy_identity_aliasing() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(OsString::from_vec(vec![0xff])), "content").unwrap();

        let error = collect(tmp.path()).unwrap_err();

        assert!(error.to_string().contains("not valid UTF-8"), "{error}");
    }
}
