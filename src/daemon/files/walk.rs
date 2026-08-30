//! Recursive filesystem walkers that never follow a symlink outward — the
//! same discipline as [`crate::daemon::apps::disk::dir_size`] and the
//! backup module's tree walker.

use std::fs;
use std::io;
use std::path::Path;

use super::{FileError, Result};

/// Copy `source` onto `destination` recursively. A symlink is recreated as a
/// symlink pointing at the same raw target — its contents are never read,
/// and the walker never follows one further down. Returns
/// `(files copied, bytes copied)`; directories are not counted as files.
pub fn copy_recursive(source: &Path, destination: &Path) -> Result<(u32, u64)> {
    let meta = fs::symlink_metadata(source).map_err(|e| FileError::io(source, e))?;
    if meta.file_type().is_symlink() {
        let target = fs::read_link(source).map_err(|e| FileError::io(source, e))?;
        std::os::unix::fs::symlink(&target, destination)
            .map_err(|e| FileError::io(destination, e))?;
        return Ok((1, 0));
    }
    if meta.is_dir() {
        fs::create_dir_all(destination).map_err(|e| FileError::io(destination, e))?;
        let mut files = 0u32;
        let mut bytes = 0u64;
        for entry in fs::read_dir(source).map_err(|e| FileError::io(source, e))? {
            let entry = entry.map_err(|e| FileError::io(source, e))?;
            let (f, b) = copy_recursive(&entry.path(), &destination.join(entry.file_name()))?;
            files += f;
            bytes += b;
        }
        return Ok((files, bytes));
    }
    fs::copy(source, destination).map_err(|e| FileError::io(source, e))?;
    Ok((1, meta.len()))
}

/// Add `path` to `builder` under `rel_name`, recursing into a directory and
/// skipping any symlink encountered — inside `path` itself or as `path`
/// itself. Returns the number of file members written.
pub fn append_named(
    builder: &mut tar::Builder<impl io::Write>,
    path: &Path,
    rel_name: &str,
) -> Result<u32> {
    let file_type = fs::symlink_metadata(path)
        .map_err(|e| FileError::io(path, e))?
        .file_type();
    if file_type.is_symlink() {
        return Ok(0);
    }
    if file_type.is_dir() {
        return append_dir(builder, path, rel_name);
    }
    let mut file = fs::File::open(path).map_err(|e| FileError::io(path, e))?;
    builder
        .append_file(rel_name, &mut file)
        .map_err(|e| FileError::io(path, e))?;
    Ok(1)
}

fn append_dir(
    builder: &mut tar::Builder<impl io::Write>,
    dir: &Path,
    rel_prefix: &str,
) -> Result<u32> {
    let mut count = 0u32;
    for entry in fs::read_dir(dir).map_err(|e| FileError::io(dir, e))? {
        let entry = entry.map_err(|e| FileError::io(dir, e))?;
        let file_type = entry
            .file_type()
            .map_err(|e| FileError::io(&entry.path(), e))?;
        if file_type.is_symlink() {
            continue;
        }
        let rel = format!("{rel_prefix}/{}", entry.file_name().to_string_lossy());
        let path = entry.path();
        if file_type.is_dir() {
            count += append_dir(builder, &path, &rel)?;
        } else {
            let mut file = fs::File::open(&path).map_err(|e| FileError::io(&path, e))?;
            builder
                .append_file(&rel, &mut file)
                .map_err(|e| FileError::io(&path, e))?;
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_recursive_recreates_symlinks_without_following_them() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        fs::write(src.path().join("a.txt"), b"hello").unwrap();
        fs::create_dir(src.path().join("sub")).unwrap();
        fs::write(src.path().join("sub/b.txt"), b"world").unwrap();

        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.bin"), [0u8; 999]).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.bin"), src.path().join("link"))
            .unwrap();

        let target = dst.path().join("copy");
        let (files, bytes) = copy_recursive(src.path(), &target).unwrap();
        // a.txt (5) + sub/b.txt (5) counted; the symlink counts as one entry
        // but contributes zero bytes — its target is never read.
        assert_eq!(files, 3);
        assert_eq!(bytes, 10);
        assert_eq!(fs::read(target.join("a.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(target.join("sub/b.txt")).unwrap(), b"world");
        let link_meta = fs::symlink_metadata(target.join("link")).unwrap();
        assert!(link_meta.file_type().is_symlink());
    }

    #[test]
    fn append_named_skips_symlinks_and_recurses_directories() {
        let src = tempfile::tempdir().unwrap();
        fs::create_dir(src.path().join("dir")).unwrap();
        fs::write(src.path().join("dir/a.txt"), b"1").unwrap();
        fs::write(src.path().join("dir/b.txt"), b"22").unwrap();
        std::os::unix::fs::symlink("/etc/hostname", src.path().join("dir/link")).unwrap();

        let archive_path = tempfile::NamedTempFile::new().unwrap();
        let file = fs::File::create(archive_path.path()).unwrap();
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            file,
            flate2::Compression::default(),
        ));
        let count = append_named(&mut builder, &src.path().join("dir"), "dir").unwrap();
        builder.into_inner().unwrap().finish().unwrap();
        assert_eq!(count, 2);

        let file = fs::File::open(archive_path.path()).unwrap();
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
        let names: Vec<String> = archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"dir/a.txt".to_string()));
        assert!(names.contains(&"dir/b.txt".to_string()));
        assert!(!names.iter().any(|n| n.contains("link")));
    }
}
