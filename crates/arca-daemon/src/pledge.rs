//! `pledge(2)`/`unveil(2)` wrappers. No-op on non-OpenBSD.

#[cfg(target_os = "openbsd")]
mod imp {
    use std::ffi::CString;
    use std::os::raw::c_char;

    unsafe extern "C" {
        fn pledge(promises: *const c_char, execpromises: *const c_char) -> i32;
        fn unveil(path: *const c_char, permissions: *const c_char) -> i32;
    }

    pub fn pledge_promises(promises: &str) -> std::io::Result<()> {
        let p = CString::new(promises).expect("nul-free");
        let rc = unsafe { pledge(p.as_ptr(), std::ptr::null()) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    pub fn unveil_path(path: &str, perms: &str) -> std::io::Result<()> {
        let p = CString::new(path).expect("nul-free");
        let m = CString::new(perms).expect("nul-free");
        let rc = unsafe { unveil(p.as_ptr(), m.as_ptr()) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    pub fn unveil_finalize() -> std::io::Result<()> {
        let rc = unsafe { unveil(std::ptr::null(), std::ptr::null()) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

#[cfg(not(target_os = "openbsd"))]
mod imp {
    pub fn pledge_promises(_promises: &str) -> std::io::Result<()> {
        tracing::debug!("pledge: no-op on non-OpenBSD");
        Ok(())
    }
    pub fn unveil_path(_path: &str, _perms: &str) -> std::io::Result<()> {
        tracing::debug!("unveil: no-op on non-OpenBSD");
        Ok(())
    }
    pub fn unveil_finalize() -> std::io::Result<()> {
        Ok(())
    }
}

pub use imp::{pledge_promises, unveil_finalize, unveil_path};
