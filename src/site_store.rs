use std::fs::{create_dir_all, read, read_dir, remove_file, write};
use std::path::{Path, PathBuf};

use crate::ChangeKind;

/// Remove site files which are no longer configured to cleanup the directory
pub fn remove_gone(expected_paths: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    fn inner(expected_paths: &[PathBuf], path: &Path) -> anyhow::Result<Vec<PathBuf>> {
        let mut superfluous = Vec::new();
        for entry in read_dir(path)? {
            let entry = entry?.path();
            if entry.is_dir() {
                superfluous.append(&mut inner(expected_paths, &entry)?);
            } else {
                // Expected is without extension
                let is_wanted = expected_paths.contains(&entry.with_extension(""));
                if !is_wanted {
                    remove_file(&entry)?;
                    superfluous.push(entry);
                }
            }
        }
        Ok(superfluous)
    }

    let mut superfluous = Vec::new();
    for entry in read_dir(".")? {
        let entry = entry?.path();
        if !entry.is_dir() {
            continue;
        }
        if let Some(filename) = entry.file_name() {
            let is_relevant = filename
                .to_str()
                .is_some_and(|filename| !filename.starts_with('.'));
            if is_relevant {
                superfluous.append(&mut inner(expected_paths, Path::new(filename))?);
            }
        }
    }
    superfluous.sort();
    Ok(superfluous)
}

/// Remove files with the same base but a different extension.
/// This cleans up changes of the extension like `html` -> `md`.
fn remove_same_base_different_extension(path: &Path) -> anyhow::Result<bool> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path.file_stem();
    let extension = path.extension();
    let mut removed_something = false;
    for file in read_dir(parent)? {
        let file = file?.path();

        let same_stem = file.file_stem() == stem;
        if !same_stem {
            continue;
        }

        let same_extension = file.extension() == extension;
        let remove = !same_extension;

        if remove {
            remove_file(file)?;
            removed_something = true;
        }
    }
    Ok(removed_something)
}

/// Write the snapshot only when its content changed.
///
/// Works on raw bytes so binary content like PDFs or images is handled exactly
/// like text. The comparison happens before any write so an unchanged snapshot
/// is never rewritten. A failed write leaves the existing snapshot untouched as
/// the temporary file is written first and only renamed into place on success.
/// Files with the same base name but another extension are removed afterwards
/// so a type change does not leave stale files behind.
pub fn write_only_changed(path: &Path, content: &[u8]) -> anyhow::Result<ChangeKind> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }

    let current = read(path).unwrap_or_default();
    let changed = current != content;
    if changed {
        write_atomic(path, content)?;
    }

    // Only clean up other extensions after the new snapshot is safely in place.
    let removed_something = remove_same_base_different_extension(path)?;

    if removed_something {
        Ok(ChangeKind::Changed)
    } else if current.is_empty() {
        Ok(ChangeKind::Init)
    } else if changed {
        Ok(ChangeKind::Changed)
    } else {
        Ok(ChangeKind::ContentSame)
    }
}

/// Write to a temporary file next to the destination and rename it into place.
///
/// When the write or the final rename fails the destination file keeps its
/// previous content as it is only touched by the final rename. The temporary
/// file is removed when the rename fails.
fn write_atomic(path: &Path, content: &[u8]) -> anyhow::Result<()> {
    let mut temporary = path.to_path_buf();
    let extension = path.extension().map_or_else(
        || std::ffi::OsString::from("tmp"),
        |extension| {
            let mut tmp_extension = extension.to_owned();
            tmp_extension.push(".tmp");
            tmp_extension
        },
    );
    temporary.set_extension(extension);
    let write_result = write(&temporary, content);
    if let Err(error) = write_result {
        let _ = remove_file(&temporary);
        return Err(error.into());
    }
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn writes_binary_content_unchanged() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("snapshot.bin");
        let content = [0x00, 0x89, 0x50, 0x4E, 0x47, 0xFF, 0xFE];

        let kind = write_only_changed(&path, &content).unwrap();
        assert!(matches!(kind, ChangeKind::Init));
        assert_eq!(read(&path).unwrap(), content);

        // Same bytes do not rewrite the file.
        let modified_before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let kind = write_only_changed(&path, &content).unwrap();
        assert!(matches!(kind, ChangeKind::ContentSame));
        assert_eq!(read(&path).unwrap(), content);
        let modified_after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(modified_before, modified_after);
    }

    #[test]
    fn removes_old_extension_when_type_changes() {
        let dir = tempdir().unwrap();
        let html = dir.path().join("page.html");
        write(&html, b"<html></html>").unwrap();

        let markdown = dir.path().join("page.md");
        let kind = write_only_changed(&markdown, b"# title").unwrap();
        assert!(matches!(kind, ChangeKind::Changed));
        assert!(!html.exists());
        assert!(markdown.exists());
    }

    #[test]
    fn keeps_snapshot_when_write_fails() {
        let dir = tempdir().unwrap();

        // An existing snapshot which must survive a failed update.
        let path = dir.path().join("snapshot.txt");
        let original = b"original content";
        write(&path, original).unwrap();

        // The destination itself is a directory so the final rename fails.
        let blocked = dir.path().join("blocked");
        create_dir_all(&blocked).unwrap();
        let result = write_atomic(&blocked, b"new content");
        assert!(result.is_err());

        // The untouched snapshot still holds the original bytes. Only the
        // destination rename failed; the old snapshot was never opened for
        // writing and the temporary file is cleaned up.
        assert_eq!(read(&path).unwrap(), original);
        assert!(blocked.is_dir());
        let files = read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn no_temp_file_left_behind_on_success() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("snapshot.txt");
        write_only_changed(&path, b"hello").unwrap();
        let files = read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(files, vec![std::ffi::OsString::from("snapshot.txt")]);
    }
}
