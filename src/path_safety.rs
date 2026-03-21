use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Debug)]
pub struct PathSafetyError {
    path: PathBuf,
    source: std::io::Error,
}

impl fmt::Display for PathSafetyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "path_canonicalize_failed: {} ({})",
            self.path.display(),
            self.source
        )
    }
}

impl std::error::Error for PathSafetyError {}

pub fn canonicalize(path: &Path) -> Result<PathBuf, PathSafetyError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };

    let mut components = absolute.components();
    let mut resolved = match components.next() {
        Some(Component::Prefix(prefix)) => {
            let mut root = PathBuf::from(prefix.as_os_str());
            if matches!(components.next(), Some(Component::RootDir)) {
                root.push(std::path::MAIN_SEPARATOR.to_string());
            }
            root
        }
        Some(Component::RootDir) => PathBuf::from(std::path::MAIN_SEPARATOR.to_string()),
        Some(other) => PathBuf::from(other.as_os_str()),
        None => PathBuf::new(),
    };

    let remaining: Vec<OsString> = components
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_os_string()),
            Component::CurDir => None,
            Component::ParentDir => Some(OsString::from("..")),
            _ => None,
        })
        .collect();

    let mut index = 0usize;
    while index < remaining.len() {
        let segment = &remaining[index];
        let candidate = resolved.join(segment);

        match fs::symlink_metadata(&candidate) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let target = fs::read_link(&candidate).map_err(|source| PathSafetyError {
                    path: candidate.clone(),
                    source,
                })?;
                resolved = if target.is_absolute() {
                    target
                } else {
                    resolved.join(target)
                };
            }
            Ok(_) => {
                resolved = candidate;
                index += 1;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                for tail in &remaining[index..] {
                    resolved.push(tail);
                }
                return Ok(resolved);
            }
            Err(source) => {
                return Err(PathSafetyError {
                    path: candidate,
                    source,
                });
            }
        }
    }

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::canonicalize;
    use std::path::Path;

    #[test]
    fn canonicalize_keeps_nonexistent_suffix() {
        let path = canonicalize(Path::new("./target/does-not-need-to-exist"))
            .expect("canonicalization should work");
        assert!(path.ends_with("target/does-not-need-to-exist"));
    }
}
