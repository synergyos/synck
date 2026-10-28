use std::path::{Path, PathBuf};

/// Advisory only: the canonical format has no whole-path length limit.
pub(crate) fn long_paths<'a>(root: &Path, paths: impl Iterator<Item = &'a str>) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        paths
            .map(|path| root.join(path))
            .filter(|path| {
                exceeds_legacy_limit(&path.as_os_str().encode_wide().collect::<Vec<_>>())
            })
            .collect()
    }
    #[cfg(not(windows))]
    {
        let _ = (root, paths);
        Vec::new()
    }
}

#[cfg(any(windows, test))]
fn exceeds_legacy_limit(path: &[u16]) -> bool {
    // Count the ordinary absolute spelling, without the extended-path prefix.
    // A verbatim UNC path replaces two leading slashes with eight units.
    const VERBATIM: &[u16] = &[92, 92, 63, 92];
    const VERBATIM_UNC: &[u16] = &[92, 92, 63, 92, 85, 78, 67, 92];
    let units = if path.starts_with(VERBATIM_UNC) {
        path.len() - 6
    } else if path.starts_with(VERBATIM) {
        path.len() - 4
    } else {
        path.len()
    };
    units > 260
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_counts_utf16_and_excludes_verbatim_prefix() {
        for prefix in ["C:\\", "\\\\server\\share\\"] {
            let path = format!("{prefix}{}", "a".repeat(260 - prefix.len()));
            assert!(!exceeds_legacy_limit(
                &path.encode_utf16().collect::<Vec<_>>()
            ));
            let path = format!("{path}😀");
            assert!(exceeds_legacy_limit(
                &path.encode_utf16().collect::<Vec<_>>()
            ));
            let extended = if let Some(unc) = path.strip_prefix("\\\\") {
                format!("\\\\?\\UNC\\{unc}")
            } else {
                format!("\\\\?\\{path}")
            };
            assert!(exceeds_legacy_limit(
                &extended.encode_utf16().collect::<Vec<_>>()
            ));
        }
    }
}
