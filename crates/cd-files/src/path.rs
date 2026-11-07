use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetPlatform {
    Linux,
    MacOS,
    Windows,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PathError {
    #[error("path must be relative")]
    Absolute,
    #[error("path is empty")]
    Empty,
    #[error("path contains a parent traversal (`..`)")]
    ParentTraversal,
    #[error("path component contains `:`")]
    Colon,
    #[error("path contains NUL")]
    Nul,
    #[error("root name must be a single component")]
    Separator,
}

/// Canonical manifest path: `/` separators, no absolute prefix, no `.`/`..`.
pub fn normalize_rel_path(input: &str) -> Result<String, PathError> {
    if input.contains('\0') {
        return Err(PathError::Nul);
    }
    let replaced = input.replace('\\', "/");
    if replaced.starts_with('/') {
        return Err(PathError::Absolute);
    }

    let mut parts = Vec::new();
    for part in replaced.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return Err(PathError::ParentTraversal);
        }
        if part.contains(':') {
            return Err(PathError::Colon);
        }
        parts.push(part.to_owned());
    }

    if parts.is_empty() {
        return Err(PathError::Empty);
    }
    Ok(parts.join("/"))
}

pub fn normalize_root_name(input: &str) -> Result<String, PathError> {
    let name = input.trim();
    if name.contains('\0') {
        return Err(PathError::Nul);
    }
    if name.is_empty() {
        return Err(PathError::Empty);
    }
    if name.contains('/') || name.contains('\\') {
        return Err(PathError::Separator);
    }
    if name == ".." || name == "." {
        return Err(PathError::ParentTraversal);
    }
    if name.contains(':') {
        return Err(PathError::Colon);
    }
    Ok(name.to_owned())
}

/// Turn one received path component into something the target accepts.
/// Returns `_` when the component becomes empty.
pub fn sanitize_component(component: &str, target: TargetPlatform) -> String {
    let mut name = component.trim().to_owned();
    if target == TargetPlatform::MacOS {
        name = name.nfc().collect();
    }

    name = name
        .chars()
        .map(|character| {
            if character.is_control() || character == '/' || character == '\\' {
                '_'
            } else {
                character
            }
        })
        .collect();

    if target == TargetPlatform::Windows {
        name = name
            .chars()
            .map(|character| match character {
                '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
                other => other,
            })
            .collect();
        name = name.trim_end_matches(['.', ' ']).to_owned();
        if is_windows_reserved(&name) {
            name = format!("_{name}");
        }
    }

    if name.is_empty() {
        "_".to_owned()
    } else {
        name
    }
}

pub fn sanitize_rel_path(input: &str, target: TargetPlatform) -> Result<String, PathError> {
    let normalized = normalize_rel_path(input)?;
    Ok(normalized
        .split('/')
        .map(|component| sanitize_component(component, target))
        .collect::<Vec<_>>()
        .join("/"))
}

/// Comparison key used for duplicate and case-collision detection.
pub fn path_key(path: &str, target: TargetPlatform) -> String {
    let normalized = if target == TargetPlatform::MacOS {
        path.nfc().collect::<String>()
    } else {
        path.to_owned()
    };
    match target {
        TargetPlatform::Linux => normalized,
        TargetPlatform::MacOS | TargetPlatform::Windows => normalized.to_lowercase(),
    }
}

fn is_windows_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    let Some(rest) = stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
    else {
        return false;
    };
    matches!(rest, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_paths_are_normalized() {
        assert_eq!(normalize_rel_path("a/b\\c.txt").unwrap(), "a/b/c.txt");
        assert_eq!(normalize_rel_path("a//b/./c").unwrap(), "a/b/c");
    }

    #[test]
    fn unsafe_paths_are_rejected() {
        assert_eq!(
            normalize_rel_path("../etc"),
            Err(PathError::ParentTraversal)
        );
        assert_eq!(normalize_rel_path("/etc/passwd"), Err(PathError::Absolute));
        assert_eq!(normalize_rel_path("C:\\Windows"), Err(PathError::Colon));
        assert_eq!(
            normalize_rel_path("a/../b"),
            Err(PathError::ParentTraversal)
        );
        assert_eq!(normalize_rel_path(""), Err(PathError::Empty));
    }

    #[test]
    fn windows_components_are_sanitised() {
        assert_eq!(sanitize_component("a:b", TargetPlatform::Windows), "a_b");
        assert_eq!(
            sanitize_component("name. ", TargetPlatform::Windows),
            "name"
        );
        assert_eq!(
            sanitize_component("CON.txt", TargetPlatform::Windows),
            "_CON.txt"
        );
        assert_eq!(sanitize_component("COM1", TargetPlatform::Windows), "_COM1");
        assert_eq!(
            sanitize_component("正常.txt", TargetPlatform::Windows),
            "正常.txt"
        );
    }

    #[test]
    fn macos_components_are_nfc() {
        let decomposed = "e\u{301}";
        assert_eq!(sanitize_component(decomposed, TargetPlatform::MacOS), "é");
    }

    #[test]
    fn keys_fold_case_on_windows_and_macos() {
        assert_eq!(
            path_key("Docs/Report.PDF", TargetPlatform::Windows),
            "docs/report.pdf"
        );
        assert_ne!(
            path_key("Docs/Report.PDF", TargetPlatform::Linux),
            "docs/report.pdf"
        );
    }

    #[test]
    fn root_names_must_be_single_components() {
        assert!(normalize_root_name("assets").is_ok());
        assert_eq!(normalize_root_name("a/b"), Err(PathError::Separator));
        assert_eq!(normalize_root_name("  "), Err(PathError::Empty));
    }
}
