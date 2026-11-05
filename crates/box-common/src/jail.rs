use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};

use smallvec::SmallVec;

const MAX_PATH_BYTES: usize = 4096;

/// Errors produced while resolving a user-supplied path against the workspace jail.
#[derive(Debug, thiserror::Error)]
pub enum JailError {
    #[error("path is empty")]
    Empty,
    #[error("path is outside the workspace jail")]
    Escape,
    #[error("path is too long")]
    TooLong,
    #[error("path contains invalid characters")]
    Invalid,
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

/// Resolve `user_path` so the result is strictly inside `root`.
///
/// Relative paths are joined to `root`. Absolute paths must already live under
/// `root`. Lexical `..` and symlink hops that leave the jail are rejected.
///
/// For paths that do not exist yet (writes), the nearest existing ancestor is
/// canonicalized and the remainder is appended only if it contains no `..`.
pub fn resolve_in_jail(root: &Path, user_path: &str) -> Result<PathBuf, JailError> {
    if user_path.contains('\0') {
        return Err(JailError::Invalid);
    }
    if user_path.len() > MAX_PATH_BYTES {
        return Err(JailError::TooLong);
    }

    let root = canonicalize_root(root)?;
    resolve_against_canonical_root(&root, user_path)
}

/// Like [`resolve_in_jail`], but `root` must already exist and be canonical.
///
/// Callers that cache `workspace.canonicalize()` (box-exec) skip a
/// `create_dir_all` + `canonicalize` on every request.
pub fn resolve_in_canonical_jail(root: &Path, user_path: &str) -> Result<PathBuf, JailError> {
    if user_path.contains('\0') {
        return Err(JailError::Invalid);
    }
    if user_path.len() > MAX_PATH_BYTES {
        return Err(JailError::TooLong);
    }
    if root.as_os_str().is_empty() {
        return Err(JailError::Empty);
    }
    resolve_against_canonical_root(root, user_path)
}

fn resolve_against_canonical_root(root: &Path, user_path: &str) -> Result<PathBuf, JailError> {
    if user_path.is_empty() {
        return Ok(root.to_path_buf());
    }

    let raw = Path::new(user_path);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        root.join(raw)
    };

    let lexical = normalize_lexical(&joined);
    if !is_inside(&lexical, root) {
        return Err(JailError::Escape);
    }

    if lexical.exists() {
        let canon = lexical.canonicalize()?;
        if !is_inside(&canon, root) {
            return Err(JailError::Escape);
        }
        return Ok(canon);
    }

    let mut ancestor = lexical.clone();
    let mut missing = SmallVec::<[OsString; 8]>::new();
    while !ancestor.exists() {
        let name = ancestor
            .file_name()
            .ok_or(JailError::Escape)?
            .to_os_string();
        if name == ".." || name == "." {
            return Err(JailError::Escape);
        }
        missing.push(name);
        ancestor = ancestor
            .parent()
            .map(Path::to_path_buf)
            .ok_or(JailError::Escape)?;
        if missing.len() > 64 {
            return Err(JailError::Escape);
        }
    }

    let canon_ancestor = ancestor.canonicalize()?;
    if !is_inside(&canon_ancestor, root) {
        return Err(JailError::Escape);
    }

    missing.reverse();
    let mut result = canon_ancestor;
    for part in missing {
        result.push(part);
        if !is_inside(&result, root) {
            return Err(JailError::Escape);
        }
    }
    Ok(result)
}

fn canonicalize_root(root: &Path) -> Result<PathBuf, JailError> {
    if root.as_os_str().is_empty() {
        return Err(JailError::Empty);
    }
    std::fs::create_dir_all(root)?;
    Ok(root.canonicalize()?)
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

fn is_inside(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn tmp_root() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn canonical_jail_matches_resolve_in_jail() {
        let tmp = tmp_root();
        fs::write(tmp.path().join("ok.txt"), b"hi").unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let a = resolve_in_jail(tmp.path(), "ok.txt").unwrap();
        let b = resolve_in_canonical_jail(&root, "ok.txt").unwrap();
        assert_eq!(a, b);
        assert!(matches!(
            resolve_in_canonical_jail(&root, "../etc/passwd"),
            Err(JailError::Escape)
        ));
    }

    #[test]
    fn nested_relative_ok() {
        let tmp = tmp_root();
        fs::create_dir_all(tmp.path().join("a/b")).unwrap();
        let resolved = resolve_in_jail(tmp.path(), "a/b/c.txt").unwrap();
        assert_eq!(
            resolved,
            tmp.path().canonicalize().unwrap().join("a/b/c.txt")
        );
    }

    #[test]
    fn absolute_inside_ok() {
        let tmp = tmp_root();
        // Canonical, so the absolute path is inside the jail where temp dirs sit behind a symlink.
        let abs = tmp.path().canonicalize().unwrap().join("inside.txt");
        fs::write(&abs, b"x").unwrap();
        let resolved = resolve_in_jail(tmp.path(), abs.to_str().unwrap()).unwrap();
        assert_eq!(resolved, abs.canonicalize().unwrap());
    }

    #[test]
    fn empty_is_root() {
        let tmp = tmp_root();
        let resolved = resolve_in_jail(tmp.path(), "").unwrap();
        assert_eq!(resolved, tmp.path().canonicalize().unwrap());
    }

    #[test]
    fn lexical_parent_escape_rejected() {
        let tmp = tmp_root();
        assert!(matches!(
            resolve_in_jail(tmp.path(), "../etc/passwd"),
            Err(JailError::Escape)
        ));
        assert!(matches!(
            resolve_in_jail(tmp.path(), "foo/../../etc/passwd"),
            Err(JailError::Escape)
        ));
    }

    #[test]
    fn absolute_outside_rejected() {
        let tmp = tmp_root();
        assert!(matches!(
            resolve_in_jail(tmp.path(), "/etc/passwd"),
            Err(JailError::Escape)
        ));
    }

    #[test]
    fn prefix_sibling_not_confused() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("workspace");
        let evil = parent.path().join("workspace-evil");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&evil).unwrap();
        fs::write(evil.join("secret"), b"nope").unwrap();
        assert!(matches!(
            resolve_in_jail(&root, evil.join("secret").to_str().unwrap()),
            Err(JailError::Escape)
        ));
    }

    #[test]
    fn symlink_escape_rejected() {
        let tmp = tmp_root();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), b"nope").unwrap();
        symlink(outside.path().join("secret"), tmp.path().join("link")).unwrap();
        assert!(matches!(
            resolve_in_jail(tmp.path(), "link"),
            Err(JailError::Escape)
        ));
    }

    #[test]
    fn symlink_inside_ok() {
        let tmp = tmp_root();
        fs::write(tmp.path().join("real.txt"), b"ok").unwrap();
        symlink(tmp.path().join("real.txt"), tmp.path().join("link.txt")).unwrap();
        let resolved = resolve_in_jail(tmp.path(), "link.txt").unwrap();
        assert_eq!(
            resolved,
            tmp.path().canonicalize().unwrap().join("real.txt")
        );
    }

    #[test]
    fn null_byte_rejected() {
        let tmp = tmp_root();
        assert!(matches!(
            resolve_in_jail(tmp.path(), "foo\0bar"),
            Err(JailError::Invalid)
        ));
    }

    #[test]
    fn too_long_rejected() {
        let tmp = tmp_root();
        let long = "a".repeat(MAX_PATH_BYTES + 1);
        assert!(matches!(
            resolve_in_jail(tmp.path(), &long),
            Err(JailError::TooLong)
        ));
    }

    #[test]
    fn dot_slash_stays_inside() {
        let tmp = tmp_root();
        fs::write(tmp.path().join("ok.txt"), b"hi").unwrap();
        let resolved = resolve_in_jail(tmp.path(), "./ok.txt").unwrap();
        assert_eq!(resolved, tmp.path().canonicalize().unwrap().join("ok.txt"));
    }
}
