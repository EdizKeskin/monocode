use tauri::WebviewWindow;

/// Task-2 contract: invoke `set_keep_awake` with `{ enabled: boolean }` from
/// each workspace window when its own agent activity changes. Repeated values
/// are safe. A window's `false` only releases that window's claim. On other
/// platforms the command succeeds without changing system power behavior.
#[tauri::command]
pub fn set_keep_awake(
    window: WebviewWindow,
    enabled: bool,
    state: tauri::State<'_, KeepAwakeState>,
) -> Result<(), String> {
    state.set(window.label(), enabled)
}

#[cfg(windows)]
mod platform {
    use std::collections::HashSet;
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::sync::Mutex;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Power::{
        PowerClearRequest, PowerCreateRequest, PowerRequestSystemRequired, PowerSetRequest,
    };
    use windows_sys::Win32::System::Threading::{
        POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
    };

    trait RequestBackend {
        type Handle;

        fn acquire(&self) -> Result<Self::Handle, String>;
        fn release(&self, handle: Self::Handle) -> Result<(), String>;
    }

    struct WindowsPowerBackend;

    impl RequestBackend for WindowsPowerBackend {
        type Handle = OwnedHandle;

        fn acquire(&self) -> Result<OwnedHandle, String> {
            let mut reason: Vec<u16> = "MonoCode agent is working\0".encode_utf16().collect();
            let context = REASON_CONTEXT {
                Version: 0,
                Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
                Reason: REASON_CONTEXT_0 {
                    SimpleReasonString: reason.as_mut_ptr(),
                },
            };
            let raw = unsafe { PowerCreateRequest(&context) };
            if raw == INVALID_HANDLE_VALUE {
                return Err(format!(
                    "PowerCreateRequest: {}",
                    io::Error::last_os_error()
                ));
            }
            // OwnedHandle closes the request even if PowerSetRequest fails.
            let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
            if unsafe { PowerSetRequest(handle.as_raw_handle(), PowerRequestSystemRequired) } == 0 {
                return Err(format!("PowerSetRequest: {}", io::Error::last_os_error()));
            }
            Ok(handle)
        }

        fn release(&self, handle: OwnedHandle) -> Result<(), String> {
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

    pub struct KeepAwakeState(Mutex<Claims<WindowsPowerBackend>>);

    impl KeepAwakeState {
        pub fn new() -> Self {
            Self(Mutex::new(Claims::new(WindowsPowerBackend)))
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
}

#[cfg(not(windows))]
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
