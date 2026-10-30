//! File-clone primitive.

use std::io;
use std::path::Path;

/// Clones `src` to the new path `dst` as a copy-on-write file. Fails if `dst`
/// exists. Returns `ErrorKind::Unsupported` where the filesystem or platform
/// cannot clone.
pub fn clone_file(src: &Path, dst: &Path) -> io::Result<()> {
    imp::clone_file(src, dst)
}

/// Whether `a` and `b` are on the same volume (equal `st_dev`). Always
/// `false` off Unix.
pub fn same_volume(a: &Path, b: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(std::fs::metadata(a)?.dev() == std::fs::metadata(b)?.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        Ok(false)
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::CString;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// `CLONE_NOFOLLOW` from <sys/clonefile.h>; the libc crate omits it.
    const CLONE_NOFOLLOW: u32 = 0x0001;

    pub fn clone_file(src: &Path, dst: &Path) -> io::Result<()> {
        let c = |p: &Path| CString::new(p.as_os_str().as_bytes()).map_err(io::Error::other);
        let (src, dst) = (c(src)?, c(dst)?);
        // SAFETY: both are valid NUL-terminated paths for the call's duration.
        let rc = unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), CLONE_NOFOLLOW) };
        if rc == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::ENOTSUP) | Some(libc::EXDEV) => {
                Err(io::Error::from(io::ErrorKind::Unsupported))
            }
            _ => Err(err),
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::fs::{self, OpenOptions};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::path::Path;

    pub fn clone_file(src: &Path, dst: &Path) -> io::Result<()> {
        let src_file = fs::File::open(src)?;
        let dst_file = OpenOptions::new().write(true).create_new(true).open(dst)?;
        // SAFETY: both descriptors are open for the duration of the call.
        let rc = unsafe {
            libc::ioctl(
                dst_file.as_raw_fd(),
                libc::FICLONE as _,
                src_file.as_raw_fd(),
            )
        };
        if rc == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        drop(dst_file);
        let _ = fs::remove_file(dst);
        match err.raw_os_error() {
            Some(libc::EOPNOTSUPP)
            | Some(libc::EXDEV)
            | Some(libc::EINVAL)
            | Some(libc::ENOTTY) => Err(io::Error::from(io::ErrorKind::Unsupported)),
            _ => Err(err),
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use std::io;
    use std::path::Path;

    pub fn clone_file(_src: &Path, _dst: &Path) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    fn ino(p: &Path) -> u64 {
        std::fs::metadata(p).unwrap().ino()
    }

    #[test]
    fn clone_file_copies_bytes_to_new_inode() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        std::fs::write(&a, b"hello").unwrap();
        match clone_file(&a, &b) {
            Err(e) if e.kind() == io::ErrorKind::Unsupported => return,
            r => r.unwrap(),
        }
        assert_eq!(std::fs::read(&b).unwrap(), b"hello");
        assert_ne!(ino(&a), ino(&b));
    }

    #[test]
    fn clone_file_refuses_existing_dst() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        std::fs::write(&a, b"new").unwrap();
        std::fs::write(&b, b"old").unwrap();
        assert!(clone_file(&a, &b).is_err());
        assert_eq!(std::fs::read(&b).unwrap(), b"old");
    }

    #[test]
    fn same_volume_true_within_tempdir() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"y").unwrap();
        assert!(same_volume(&a, &b).unwrap());
    }
}
