/// Reports whether a project or workspace name matches `[A-Za-z0-9_][A-Za-z0-9_-]*`.
pub(crate) fn is_valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
        && bytes.all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{error::Error, project::validate_project_name, workspace::validate_name};

    #[test]
    fn accepts_the_safe_ascii_set() {
        for name in ["main", "my-name_2", "trailing-", "_private-"] {
            assert!(is_valid_name(name));
            assert!(validate_project_name(name).is_ok());
            assert!(validate_name(name).is_ok());
        }
    }

    #[test]
    fn rejects_names_outside_the_safe_ascii_set() {
        for name in [
            "",
            "-leading",
            ".files",
            "a:b",
            "a.b",
            "owner/name",
            "two words",
            "cafe\u{301}",
        ] {
            assert!(!is_valid_name(name));
            assert!(matches!(
                validate_project_name(name),
                Err(Error::InvalidName(_))
            ));
            assert!(matches!(validate_name(name), Err(Error::InvalidName(_))));
        }
    }
}
