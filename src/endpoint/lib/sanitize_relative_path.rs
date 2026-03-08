use std::path::{Component, Path, PathBuf};

pub fn sanitize_relative_path(user_path: &str) -> Result<PathBuf, &'static str> {
    let mut resolved_path = PathBuf::from(".");
    let mut depth = 0;

    for component in Path::new(user_path).components() {
        match component {
            Component::Normal(name) => {
                resolved_path.push(name);
                depth += 1;
            }
            Component::ParentDir => {
                // If depth is 0, the user is trying to use `..` to escape `./`
                if depth == 0 {
                    return Err("Path traversal attempt detected");
                }
                resolved_path.pop();
                depth -= 1;
            }
            Component::CurDir => {} // Ignore `.` (current directory)
            Component::RootDir | Component::Prefix(_) => {
                return Err("Absolute paths are not allowed");
            }
        }
    }

    // Ensure they didn't just request the root directory itself
    if depth == 0 {
        return Err("File path cannot be empty");
    }

    Ok(resolved_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    #[test]
    fn test_valid_relative_paths() {
        assert_eq!(
            sanitize_relative_path("docs/resume.pdf").unwrap(),
            PathBuf::from("./docs/resume.pdf")
        );
        assert_eq!(
            sanitize_relative_path("image.png").unwrap(),
            PathBuf::from("./image.png")
        );
        assert_eq!(
            sanitize_relative_path("./folder/file.txt").unwrap(),
            PathBuf::from("./folder/file.txt")
        );
    }

    #[test]
    fn test_valid_parent_navigation() {
        // Moving up and back down should stay within bounds
        assert_eq!(
            sanitize_relative_path("dir/../file.txt").unwrap(),
            PathBuf::from("./file.txt")
        );
        assert_eq!(
            sanitize_relative_path("a/b/../../c").unwrap(),
            PathBuf::from("./c")
        );
    }

    #[test]
    fn test_prevents_traversal_escape() {
        // Trying to go above the root "."
        assert!(sanitize_relative_path("..").is_err());
        assert!(sanitize_relative_path("../etc/passwd").is_err());
        assert!(sanitize_relative_path("dir/../../secret").is_err());
    }

    #[test]
    fn test_rejects_absolute_paths() {
        // Linux/Unix style
        assert!(sanitize_relative_path("/etc/passwd").is_err());
        // Windows style (if compiled on Windows)
        if cfg!(windows) {
            assert!(sanitize_relative_path(r"C:\Windows\System32").is_err());
        }
    }

    #[test]
    fn test_rejects_empty_or_identity_paths() {
        assert!(sanitize_relative_path("").is_err());
        assert!(sanitize_relative_path(".").is_err());
        assert!(sanitize_relative_path("././.").is_err());
        assert!(sanitize_relative_path("dir/..").is_err()); // Results in depth 0
    }

    #[test]
    fn test_redundant_separators() {
        // Rust's Path component logic usually handles multiple slashes
        assert_eq!(
            sanitize_relative_path("folder///file.txt").unwrap(),
            PathBuf::from("./folder/file.txt")
        );
    }
}
