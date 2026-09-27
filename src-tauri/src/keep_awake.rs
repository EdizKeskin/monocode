use std::collections::HashSet;
use std::sync::Mutex;

use tauri::WebviewWindow;

const REASON: &str = "MonoCode agent is working";

/// Invoke `set_keep_awake` with `{ enabled: boolean }` from each workspace
/// window when its own agent activity changes. Repeated values are safe. A
/// window's `false` only releases that window's claim.
#[tauri::command]
pub fn set_keep_awake(
    window: WebviewWindow,
    enabled: bool,
    state: tauri::State<'_, KeepAwakeState>,
) -> Result<(), String> {
    state.set(window.label(), enabled)
}

trait RequestBackend {
    type Handle;

    fn acquire(&self) -> Result<Self::Handle, String>;
    fn release(&self, handle: Self::Handle) -> Result<(), String>;
}

struct Claims<B: RequestBackend> {
    backend: B,
    windows: HashSet<String>,
    request: Option<B::Handle>,
}

impl<B: RequestBackend> Claims<B> {
    fn new(backend: B) -> Self {
        Self {
            backend,
            windows: HashSet::new(),
            request: None,
        }
    }

    fn set(&mut self, window: &str, enabled: bool) -> Result<(), String> {
        if enabled {
            if self.windows.contains(window) {
                return Ok(());
            }
            if self.request.is_none() {
                self.request = Some(self.backend.acquire()?);
            }
            self.windows.insert(window.to_owned());
        } else if self.windows.remove(window) && self.windows.is_empty() {
            if let Some(request) = self.request.take() {
                self.backend.release(request)?;
            }
        }
        Ok(())
    }

    fn release_all(&mut self) -> Result<(), String> {
        self.windows.clear();
        if let Some(request) = self.request.take() {
            self.backend.release(request)?;
        }
        Ok(())
    }
}

#[cfg(windows)]
mod windows_backend {
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Power::{
        PowerClearRequest, PowerCreateRequest, PowerRequestSystemRequired, PowerSetRequest,
    };
    use windows_sys::Win32::System::Threading::{
        POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
    };

    use super::{RequestBackend, REASON};

    pub struct WindowsPowerBackend;

    impl RequestBackend for WindowsPowerBackend {
        type Handle = OwnedHandle;

        fn acquire(&self) -> Result<OwnedHandle, String> {
            let mut reason: Vec<u16> = format!("{REASON}\0").encode_utf16().collect();
            let context = REASON_CONTEXT {
                Version: 0,
                Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
                Reason: REASON_CONTEXT_0 {
                    SimpleReasonString: reason.as_mut_ptr(),
                },
            };
            // SAFETY: `reason` stays alive for the PowerCreateRequest call.
            let raw = unsafe { PowerCreateRequest(&context) };
            if raw == INVALID_HANDLE_VALUE {
                return Err(format!(
                    "PowerCreateRequest: {}",
                    io::Error::last_os_error()
                ));
            }
            // OwnedHandle closes the request even if PowerSetRequest fails.
            // SAFETY: PowerCreateRequest returned a valid handle.
            let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
            // SAFETY: handle is an open power request created above.
            if unsafe { PowerSetRequest(handle.as_raw_handle(), PowerRequestSystemRequired) } == 0 {
                return Err(format!("PowerSetRequest: {}", io::Error::last_os_error()));
            }
            Ok(handle)
        }

        fn release(&self, handle: OwnedHandle) -> Result<(), String> {
            // SAFETY: handle is the power request acquired by this backend.
            let result =
                unsafe { PowerClearRequest(handle.as_raw_handle(), PowerRequestSystemRequired) };
            let error = if result == 0 {
                Some(io::Error::last_os_error())
            } else {
                None
            };
            // Closing the handle removes the request even if clearing failed.
            drop(handle);
            match error {
                Some(error) => Err(format!("PowerClearRequest: {error}")),
                None => Ok(()),
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod macos_backend {
    use std::ffi::{c_char, c_void, CString};
    use std::ptr;

    use super::{RequestBackend, REASON};

    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const K_IOPM_ASSERTION_LEVEL_ON: u32 = 255;
    const K_IO_RETURN_SUCCESS: i32 = 0;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            alloc: *const c_void,
            c_str: *const c_char,
            encoding: u32,
        ) -> *const c_void;
        fn CFRelease(cf: *const c_void);
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            assertion_type: *const c_void,
            assertion_level: u32,
            assertion_name: *const c_void,
            assertion_id: *mut u32,
        ) -> i32;
        fn IOPMAssertionRelease(assertion_id: u32) -> i32;
    }

    struct CfString(*const c_void);

    impl CfString {
        fn from_str(value: &str) -> Result<Self, String> {
            let c_str =
                CString::new(value).map_err(|_| "keep-awake reason contained NUL".to_string())?;
            // SAFETY: c_str is a valid C string for the duration of this call.
            let raw = unsafe {
                CFStringCreateWithCString(ptr::null(), c_str.as_ptr(), K_CF_STRING_ENCODING_UTF8)
            };
            if raw.is_null() {
                Err("CFStringCreateWithCString failed".into())
            } else {
                Ok(Self(raw))
            }
        }
    }

    impl Drop for CfString {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: 0 is a CFString created by CFStringCreateWithCString.
                unsafe { CFRelease(self.0) };
            }
        }
    }

    pub struct MacosIdleBackend;

    impl RequestBackend for MacosIdleBackend {
        type Handle = u32;

        fn acquire(&self) -> Result<u32, String> {
            let assertion_type = CfString::from_str("PreventUserIdleSystemSleep")?;
            let assertion_name = CfString::from_str(REASON)?;
            let mut assertion_id = 0u32;
            // SAFETY: both CFStrings are live, non-null, and assertion_id is writable.
            let status = unsafe {
                IOPMAssertionCreateWithName(
                    assertion_type.0,
                    K_IOPM_ASSERTION_LEVEL_ON,
                    assertion_name.0,
                    &mut assertion_id,
                )
            };
            if status != K_IO_RETURN_SUCCESS {
                return Err(format!("IOPMAssertionCreateWithName: {status}"));
            }
            Ok(assertion_id)
        }

        fn release(&self, handle: u32) -> Result<(), String> {
            // SAFETY: handle is an assertion id created by IOPMAssertionCreateWithName.
            let status = unsafe { IOPMAssertionRelease(handle) };
            if status != K_IO_RETURN_SUCCESS {
                Err(format!("IOPMAssertionRelease: {status}"))
            } else {
                Ok(())
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod linux_backend {
    use zbus::blocking::Connection;
    use zbus::zvariant::OwnedFd;

    use super::{RequestBackend, REASON};

    pub struct LinuxIdleBackend;

    impl RequestBackend for LinuxIdleBackend {
        type Handle = OwnedFd;

        fn acquire(&self) -> Result<OwnedFd, String> {
            let conn = Connection::system().map_err(|error| error.to_string())?;
            let reply = conn
                .call_method(
                    Some("org.freedesktop.login1"),
                    "/org/freedesktop/login1",
                    Some("org.freedesktop.login1.Manager"),
                    "Inhibit",
                    &("idle", "MonoCode", REASON, "block"),
                )
                .map_err(|error| error.to_string())?;
            reply
                .body()
                .deserialize()
                .map_err(|error| error.to_string())
        }

        fn release(&self, handle: OwnedFd) -> Result<(), String> {
            drop(handle);
            Ok(())
        }
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod platform {
    use super::*;

    #[cfg(windows)]
    type Backend = super::windows_backend::WindowsPowerBackend;
    #[cfg(target_os = "macos")]
    type Backend = super::macos_backend::MacosIdleBackend;
    #[cfg(target_os = "linux")]
    type Backend = super::linux_backend::LinuxIdleBackend;

    #[cfg(windows)]
    fn new_backend() -> Backend {
        super::windows_backend::WindowsPowerBackend
    }
    #[cfg(target_os = "macos")]
    fn new_backend() -> Backend {
        super::macos_backend::MacosIdleBackend
    }
    #[cfg(target_os = "linux")]
    fn new_backend() -> Backend {
        super::linux_backend::LinuxIdleBackend
    }

    pub struct KeepAwakeState(Mutex<Claims<Backend>>);

    impl KeepAwakeState {
        pub fn new() -> Self {
            Self(Mutex::new(Claims::new(new_backend())))
        }

        pub fn set(&self, window: &str, enabled: bool) -> Result<(), String> {
            match self.0.lock() {
                Ok(mut claims) => claims.set(window, enabled),
                Err(poisoned) => {
                    let _ = poisoned.into_inner().release_all();
                    Err("keep-awake state was poisoned".into())
                }
            }
        }

        pub fn window_closed(&self, window: &str) {
            if let Err(error) = self.set(window, false) {
                eprintln!("keep-awake window cleanup: {error}");
            }
        }

        pub fn shutdown(&self) {
            let result = match self.0.lock() {
                Ok(mut claims) => claims.release_all(),
                Err(poisoned) => poisoned.into_inner().release_all(),
            };
            if let Err(error) = result {
                eprintln!("keep-awake shutdown cleanup: {error}");
            }
        }
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod platform {
    pub struct KeepAwakeState;

    impl KeepAwakeState {
        pub fn new() -> Self {
            Self
        }

        pub fn set(&self, _window: &str, _enabled: bool) -> Result<(), String> {
            Ok(())
        }

        pub fn window_closed(&self, _window: &str) {}

        pub fn shutdown(&self) {}
    }
}

pub use platform::KeepAwakeState;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct MockBackend {
        acquired: AtomicUsize,
        released: AtomicUsize,
        fail_acquire: AtomicBool,
        fail_release: AtomicBool,
    }

    impl RequestBackend for MockBackend {
        type Handle = ();

        fn acquire(&self) -> Result<(), String> {
            if self.fail_acquire.load(Ordering::SeqCst) {
                return Err("acquire failed".into());
            }
            self.acquired.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn release(&self, _handle: ()) -> Result<(), String> {
            self.released.fetch_add(1, Ordering::SeqCst);
            if self.fail_release.load(Ordering::SeqCst) {
                return Err("release failed".into());
            }
            Ok(())
        }
    }

    #[test]
    fn claims_are_idempotent_and_owned_by_window() {
        let mut claims = Claims::new(MockBackend::default());
        claims.set("first", true).unwrap();
        claims.set("first", true).unwrap();
        claims.set("second", true).unwrap();
        assert_eq!(claims.backend.acquired.load(Ordering::SeqCst), 1);

        claims.set("first", false).unwrap();
        claims.set("first", false).unwrap();
        assert_eq!(claims.backend.released.load(Ordering::SeqCst), 0);
        assert!(claims.windows.contains("second"));

        claims.set("second", false).unwrap();
        assert_eq!(claims.backend.released.load(Ordering::SeqCst), 1);
        assert!(claims.request.is_none());
    }

    #[test]
    fn failed_acquire_does_not_claim_window() {
        let mut claims = Claims::new(MockBackend::default());
        claims.backend.fail_acquire.store(true, Ordering::SeqCst);
        assert!(claims.set("first", true).is_err());
        assert!(claims.windows.is_empty());
        assert!(claims.request.is_none());
        claims.backend.fail_acquire.store(false, Ordering::SeqCst);
        claims.set("first", true).unwrap();
        assert_eq!(claims.backend.acquired.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failed_release_and_shutdown_drop_the_claim() {
        let mut claims = Claims::new(MockBackend::default());
        claims.set("first", true).unwrap();
        claims.backend.fail_release.store(true, Ordering::SeqCst);
        assert!(claims.set("first", false).is_err());
        assert!(claims.windows.is_empty());
        assert!(claims.request.is_none());

        claims.backend.fail_release.store(false, Ordering::SeqCst);
        claims.set("first", true).unwrap();
        claims.set("second", true).unwrap();
        claims.release_all().unwrap();
        assert!(claims.windows.is_empty());
        assert!(claims.request.is_none());
        assert_eq!(claims.backend.released.load(Ordering::SeqCst), 2);
    }
}
