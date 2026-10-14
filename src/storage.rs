//! Filesystem-backed tarball storage.
//!
//! Layout: `{root}/{package-name}/{tarball-filename}` where a scoped name such as
//! `@acme/widgets` becomes the nested path `@acme/widgets`. Every segment is
//! validated before it is ever joined onto the root, so a malicious package name
//! can never escape the storage directory.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::error::AppError;

#[derive(Debug, Clone)]
pub struct Storage {
    root: PathBuf,
}

impl Storage {
    pub fn new(root: PathBuf) -> Self {
        // Resolve symlinks once (macOS `/tmp` -> `/private/tmp`, bind mounts, …)
        // so later containment checks compare like with like.
        let root = root.canonicalize().unwrap_or(root);
        Self { root }
    }

    /// Relative path (from the storage root) of a tarball, as persisted in the DB.
    pub fn rel_path(package: &str, filename: &str) -> Result<String, AppError> {
        validate_package_name(package).map_err(AppError::BadRequest)?;
        validate_filename(filename).map_err(AppError::BadRequest)?;
        Ok(format!("{package}/{filename}"))
    }

    /// Write a tarball. Overwrites an existing file (republish / integrity fix).
    pub fn write(&self, rel_path: &str, bytes: &[u8]) -> Result<PathBuf> {
        let full = self.abs(rel_path)?;
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create '{}'", parent.display()))?;
        }
        std::fs::write(&full, bytes)
            .with_context(|| format!("failed to write '{}'", full.display()))?;
        Ok(full)
    }

    /// Resolve a stored relative path to an absolute path, refusing to escape root.
    pub fn abs(&self, rel_path: &str) -> Result<PathBuf> {
        if rel_path.is_empty() {
            bail!("empty storage path");
        }
        let candidate = Path::new(rel_path);
        if candidate.is_absolute() {
            bail!("absolute storage paths are not allowed");
        }
        // Lexical guard: reject anything that is not a plain nested name.
        for component in candidate.components() {
            use std::path::Component;
            match component {
                Component::Normal(_) => {}
                _ => bail!("illegal component in storage path '{rel_path}'"),
            }
        }

        let full = self.root.join(candidate);

        // Physical guard: when the path already exists, confirm it really lives
        // under the root. Paths that do not exist yet cannot be canonicalised —
        // the lexical check above already covers them.
        if let Ok(canonical) = full.canonicalize() {
            if !canonical.starts_with(&self.root) {
                bail!("storage path escapes the storage root");
            }
        } else if let Some(parent) = full.parent() {
            if let Ok(canonical_parent) = parent.canonicalize() {
                if !canonical_parent.starts_with(&self.root) {
                    bail!("storage path escapes the storage root");
                }
            }
        }

        Ok(full)
    }

    pub fn remove(&self, rel_path: &str) -> Result<()> {
        let full = self.abs(rel_path)?;
        match std::fs::remove_file(&full) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("failed to delete '{}'", full.display())),
        }
    }

    /// Remove every tarball belonging to a package.
    pub fn remove_package(&self, package: &str) -> Result<()> {
        validate_package_name(package).map_err(|e| anyhow::anyhow!(e))?;
        let dir = self.root.join(package);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("failed to delete '{}'", dir.display())),
        }
    }
}

/// npm package names: lowercase, optionally scoped (`@scope/name`).
pub fn validate_package_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 214 {
        return Err("invalid package name length".into());
    }
    if name.starts_with('.') || name.starts_with('_') {
        return Err("package name may not start with '.' or '_'".into());
    }
    let (scope, base) = match name.strip_prefix('@') {
        Some(rest) => match rest.split_once('/') {
            Some((scope, base)) => (Some(scope), base),
            None => return Err("scoped package name must be '@scope/name'".into()),
        },
        None => (None, name),
    };
    if let Some(scope) = scope {
        if scope.is_empty() || scope.contains('/') {
            return Err("invalid scope".into());
        }
    }
    if base.is_empty() || base.contains('/') {
        return Err("invalid package name".into());
    }
    for ch in name.chars() {
        let ok = ch.is_ascii_lowercase()
            || ch.is_ascii_digit()
            || matches!(ch, '-' | '_' | '.' | '~' | '@' | '/');
        if !ok {
            return Err(format!("illegal character '{ch}' in package name"));
        }
    }
    if name.contains("..") {
        return Err("package name may not contain '..'".into());
    }
    Ok(())
}

/// Tarball filenames are always `<something>.tgz`.
pub fn validate_filename(filename: &str) -> Result<(), String> {
    if filename.is_empty() || filename.len() > 255 {
        return Err("invalid tarball filename length".into());
    }
    if filename.contains('/') || filename.contains('\\') || filename.contains("..") {
        return Err("tarball filename may not contain path separators".into());
    }
    if !filename.ends_with(".tgz") {
        return Err("tarball filename must end with .tgz".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_normal_names() {
        assert!(validate_package_name("lodash").is_ok());
        assert!(validate_package_name("@acme/widgets").is_ok());
        assert!(validate_package_name("some-pkg.js").is_ok());
    }

    #[test]
    fn rejects_traversal() {
        assert!(validate_package_name("../etc/passwd").is_err());
        assert!(validate_package_name("UPPER").is_err());
        assert!(validate_filename("../evil.tgz").is_err());
        assert!(validate_filename("evil.tar.gz").is_err());
    }

    #[test]
    fn abs_handles_paths_that_do_not_exist_yet() {
        // Regression: on macOS `/tmp` is a symlink, so a not-yet-created parent
        // must not be compared against a canonicalised root.
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(dir.path().to_path_buf());

        assert!(storage.abs("hello-npmreg/hello-npmreg-1.0.0.tgz").is_ok());
        assert!(storage.abs("nested/deeper/pkg.tgz").is_ok());
        assert!(storage.abs("../escape.tgz").is_err());
        assert!(storage.abs("/etc/passwd").is_err());
        assert!(storage.abs("").is_err());
    }

    #[test]
    fn write_then_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(dir.path().to_path_buf());
        let rel = Storage::rel_path("pkg", "pkg-1.0.0.tgz").unwrap();
        let written = storage.write(&rel, b"tarball-bytes").unwrap();
        assert_eq!(std::fs::read(written).unwrap(), b"tarball-bytes");
        storage.remove(&rel).unwrap();
        assert!(!storage.abs(&rel).unwrap().exists());
    }
}
