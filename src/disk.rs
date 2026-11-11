//! Free-space checks for the recording folder.

use std::path::Path;

/// Free bytes on the filesystem containing `path`, when the platform exposes
/// it. An unknown value is treated as sufficient by [`has_space`].
#[cfg(unix)]
pub fn free_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` initialises the struct it is given, and the path is a
    // valid NUL-terminated C string that outlives the call.
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return None;
    }
    let stats = unsafe { stats.assume_init() };
    Some((stats.f_frsize as u64).saturating_mul(stats.f_bavail as u64))
}

#[cfg(not(unix))]
pub fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

/// Whether at least `min_mb` megabytes are free. Zero disables the check, and
/// a platform that cannot report free space never blocks recording.
pub fn has_space(path: &Path, min_mb: u32) -> bool {
    if min_mb == 0 {
        return true;
    }
    match free_bytes(path) {
        Some(bytes) => bytes >= u64::from(min_mb) * 1024 * 1024,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_temp_directory_reports_free_space() {
        if !cfg!(unix) {
            return;
        }
        let dir = std::env::temp_dir();
        assert!(free_bytes(&dir).is_some());
        assert!(has_space(&dir, 0));
        assert!(has_space(&dir, 1));
        assert!(!has_space(&dir, u32::MAX));
    }

    #[test]
    fn a_missing_directory_is_treated_as_unknown() {
        let path = std::env::temp_dir().join("viberadio-no-such-directory-for-disk-test");
        assert!(has_space(&path, 1));
    }
}
