use std::ffi::{CStr, CString};
use std::marker::PhantomData;
use std::panic::AssertUnwindSafe;

use crate::util::Binding;
use crate::{panic, raw, Error, IndexEntry};

use libc::{c_char, c_int};

/// Result of a merge driver's `apply` operation.
pub enum MergeDriverApplyResult {
    /// Merge succeeded — provide merged content and optionally override path/mode.
    Applied {
        /// The merged file content.
        content: Vec<u8>,
        /// Optionally override the path of the merged result.
        path: Option<String>,
        /// Optionally override the file mode of the merged result.
        mode: Option<u32>,
    },
    /// Fall back to the default ("text") merge driver.
    Passthrough,
    /// File remains conflicted.
    Conflict,
}

/// Information about the merge being performed.
///
/// Wraps `git_merge_driver_source` with safe accessors for the ancestor,
/// ours, and theirs index entries.
pub struct MergeDriverSource<'a> {
    raw: *const raw::git_merge_driver_source,
    _marker: PhantomData<&'a ()>,
}

impl<'a> MergeDriverSource<'a> {
    /// Get the ancestor (common base) index entry.
    pub fn ancestor(&self) -> Option<IndexEntry> {
        unsafe {
            let ptr = raw::git_merge_driver_source_ancestor(self.raw);
            if ptr.is_null() {
                None
            } else {
                Some(Binding::from_raw(*ptr))
            }
        }
    }

    /// Get the "ours" (current branch) index entry.
    pub fn ours(&self) -> Option<IndexEntry> {
        unsafe {
            let ptr = raw::git_merge_driver_source_ours(self.raw);
            if ptr.is_null() {
                None
            } else {
                Some(Binding::from_raw(*ptr))
            }
        }
    }

    /// Get the "theirs" (branch being merged in) index entry.
    pub fn theirs(&self) -> Option<IndexEntry> {
        unsafe {
            let ptr = raw::git_merge_driver_source_theirs(self.raw);
            if ptr.is_null() {
                None
            } else {
                Some(Binding::from_raw(*ptr))
            }
        }
    }
}

impl<'a> Binding for MergeDriverSource<'a> {
    type Raw = *const raw::git_merge_driver_source;

    unsafe fn from_raw(raw: *const raw::git_merge_driver_source) -> MergeDriverSource<'a> {
        MergeDriverSource {
            raw,
            _marker: PhantomData,
        }
    }

    fn raw(&self) -> *const raw::git_merge_driver_source {
        self.raw
    }
}

/// Trait for implementing custom merge drivers.
///
/// The merge driver registry in libgit2 is not thread-safe. Drivers must be
/// registered during application startup, before any concurrent git operations.
pub trait MergeDriver: Send + Sync + 'static {
    /// Called once when the driver is first used.
    ///
    /// Override this to perform one-time initialization.
    fn initialize(&mut self) -> Result<(), Error> {
        Ok(())
    }

    /// Called when the driver is unregistered.
    ///
    /// Override this to perform cleanup.
    fn shutdown(&mut self) {}

    /// Apply the merge driver to resolve a conflict.
    ///
    /// `filter_name` is the name of the merge driver as configured in
    /// `.gitattributes`. `src` provides access to the ancestor, ours, and
    /// theirs index entries.
    fn apply(
        &mut self,
        filter_name: &str,
        src: &MergeDriverSource<'_>,
    ) -> Result<MergeDriverApplyResult, Error>;
}

/// Instance of a `git_merge_driver`. Must use `#[repr(C)]` to ensure that
/// the C fields come first so pointer casting works.
#[repr(C)]
struct RawMergeDriver {
    raw: raw::git_merge_driver,
    obj: Box<dyn MergeDriver>,
    initialized: bool,
    /// Keeps the path_out CString alive after the apply callback returns.
    last_path: Option<CString>,
}

/// Register a custom merge driver.
///
/// # Safety
///
/// This function is unsafe because the libgit2 merge driver registry is not
/// thread-safe. You must ensure that:
/// 1. No concurrent git operations are in progress
/// 2. Registration happens during application initialization
///
/// # Arguments
/// * `name` - Name to register the driver under (referenced in `.gitattributes`
///   via `merge=<name>`)
/// * `driver` - The merge driver implementation
pub unsafe fn register_merge_driver<D: MergeDriver>(
    name: &str,
    driver: D,
) -> Result<(), Error> {
    crate::init();

    let name = CString::new(name)?;

    let raw_driver = Box::new(RawMergeDriver {
        raw: raw::git_merge_driver {
            version: raw::GIT_MERGE_DRIVER_VERSION,
            initialize: Some(merge_driver_init_cb),
            shutdown: Some(merge_driver_shutdown_cb),
            apply: Some(merge_driver_apply_cb),
        },
        obj: Box::new(driver),
        initialized: false,
        last_path: None,
    });

    let raw_ptr = Box::into_raw(raw_driver);

    let rc = raw::git_merge_driver_register(
        name.as_ptr(),
        raw_ptr as *mut raw::git_merge_driver,
    );

    if rc < 0 {
        // Take back ownership so we don't leak
        let _ = Box::from_raw(raw_ptr);
        return Err(Error::last_error(rc));
    }

    // Ownership transferred to libgit2; freed in merge_driver_shutdown_cb
    Ok(())
}

/// Unregister a custom merge driver.
///
/// # Safety
///
/// Same thread-safety requirements as `register_merge_driver`.
/// This function must not be called while any git operations are in progress
/// that might use this driver.
pub unsafe fn unregister_merge_driver(name: &str) -> Result<(), Error> {
    crate::init();
    let name = CString::new(name)?;
    let rc = raw::git_merge_driver_unregister(name.as_ptr());
    if rc < 0 {
        return Err(Error::last_error(rc));
    }
    Ok(())
}

/// Look up a registered merge driver by name.
///
/// Returns `true` if a driver with the given name is registered, `false`
/// otherwise.
pub fn lookup_merge_driver(name: &str) -> bool {
    crate::init();
    let name = match CString::new(name) {
        Ok(s) => s,
        Err(_) => return false,
    };
    unsafe { !raw::git_merge_driver_lookup(name.as_ptr()).is_null() }
}

extern "C" fn merge_driver_init_cb(driver: *mut raw::git_merge_driver) -> c_int {
    unsafe {
        let raw_driver = &mut *(driver as *mut RawMergeDriver);
        if raw_driver.initialized {
            return 0;
        }
        match panic::wrap(AssertUnwindSafe(|| raw_driver.obj.initialize())) {
            Some(Ok(())) => {
                raw_driver.initialized = true;
                0
            }
            Some(Err(_)) => -1,
            None => -1,
        }
    }
}

extern "C" fn merge_driver_shutdown_cb(driver: *mut raw::git_merge_driver) {
    unsafe {
        let raw_driver = &mut *(driver as *mut RawMergeDriver);
        let _ = panic::wrap(AssertUnwindSafe(|| raw_driver.obj.shutdown()));

        // Clean up the driver — it was allocated in register_merge_driver
        let _ = Box::from_raw(driver as *mut RawMergeDriver);
    }
}

extern "C" fn merge_driver_apply_cb(
    driver: *mut raw::git_merge_driver,
    path_out: *mut *const c_char,
    mode_out: *mut u32,
    merged_out: *mut raw::git_buf,
    filter_name: *const c_char,
    src: *const raw::git_merge_driver_source,
) -> c_int {
    unsafe {
        let raw_driver = &mut *(driver as *mut RawMergeDriver);

        let filter_name_str = match CStr::from_ptr(filter_name).to_str() {
            Ok(s) => s,
            Err(_) => return -1,
        };

        let source = MergeDriverSource::from_raw(src);

        match panic::wrap(AssertUnwindSafe(|| {
            raw_driver.obj.apply(filter_name_str, &source)
        })) {
            Some(Ok(MergeDriverApplyResult::Applied {
                content,
                path,
                mode,
            })) => {
                // Write merged content to merged_out
                let rc = raw::git_buf_set(
                    merged_out,
                    content.as_ptr() as *const _,
                    content.len(),
                );
                if rc < 0 {
                    return -1;
                }

                // Optionally set the output path
                if let Some(p) = path {
                    match CString::new(p) {
                        Ok(cs) => {
                            *path_out = cs.as_ptr();
                            raw_driver.last_path = Some(cs);
                        }
                        Err(_) => return -1,
                    }
                }

                // Optionally set the output mode
                if let Some(m) = mode {
                    *mode_out = m;
                }

                0
            }
            Some(Ok(MergeDriverApplyResult::Passthrough)) => raw::GIT_PASSTHROUGH,
            Some(Ok(MergeDriverApplyResult::Conflict)) => raw::GIT_EMERGECONFLICT,
            Some(Err(_)) => -1,
            None => -1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Repository;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    struct NoopDriver;

    impl MergeDriver for NoopDriver {
        fn apply(
            &mut self,
            _filter_name: &str,
            _src: &MergeDriverSource<'_>,
        ) -> Result<MergeDriverApplyResult, Error> {
            Ok(MergeDriverApplyResult::Passthrough)
        }
    }

    #[test]
    fn test_register_unregister() {
        let _cfg = crate::init();
        unsafe {
            register_merge_driver("test-noop-register", NoopDriver).unwrap();
        }
        assert!(lookup_merge_driver("test-noop-register"));
        unsafe {
            unregister_merge_driver("test-noop-register").unwrap();
        }
        assert!(!lookup_merge_driver("test-noop-register"));
    }

    #[test]
    fn test_lookup_builtin_drivers() {
        let _cfg = crate::init();
        assert!(
            lookup_merge_driver("text"),
            "text merge driver should exist"
        );
        assert!(
            lookup_merge_driver("binary"),
            "binary merge driver should exist"
        );
        assert!(
            lookup_merge_driver("union"),
            "union merge driver should exist"
        );
        assert!(
            !lookup_merge_driver("nonexistent"),
            "nonexistent merge driver should not exist"
        );
    }

    #[test]
    fn test_custom_merge_driver_invocation() {
        let _cfg = crate::init();

        let called = Arc::new(AtomicBool::new(false));
        let called_clone = called.clone();

        struct TestDriver {
            called: Arc<AtomicBool>,
        }

        impl MergeDriver for TestDriver {
            fn apply(
                &mut self,
                _filter_name: &str,
                _src: &MergeDriverSource<'_>,
            ) -> Result<MergeDriverApplyResult, Error> {
                self.called.store(true, Ordering::SeqCst);
                Ok(MergeDriverApplyResult::Applied {
                    content: b"merged by custom driver".to_vec(),
                    path: None,
                    mode: None,
                })
            }
        }

        unsafe {
            register_merge_driver(
                "test-custom-invoke",
                TestDriver {
                    called: called_clone,
                },
            )
            .unwrap();
        }

        // Create a repo with diverging branches
        let td = tempfile::TempDir::new().unwrap();
        let repo = Repository::init(td.path()).unwrap();

        // Configure the merge driver for .txt files
        std::fs::write(
            td.path().join(".gitattributes"),
            "*.txt merge=test-custom-invoke\n",
        )
        .unwrap();

        // Create initial commit on master with a file
        std::fs::write(td.path().join("file.txt"), "base content\n").unwrap();
        {
            let mut index = repo.index().unwrap();
            index
                .add_path(std::path::Path::new(".gitattributes"))
                .unwrap();
            index.add_path(std::path::Path::new("file.txt")).unwrap();
            index.write().unwrap();
            let tree_oid = index.write_tree().unwrap();
            let tree = repo.find_tree(tree_oid).unwrap();
            let sig = crate::Signature::now("Test", "test@test.com").unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
                .unwrap();
        }

        let base_commit = repo.head().unwrap().peel_to_commit().unwrap();

        // Create "ours" branch with a change
        repo.branch("ours", &base_commit, false).unwrap();
        repo.set_head("refs/heads/ours").unwrap();
        repo.checkout_head(None).unwrap();
        std::fs::write(td.path().join("file.txt"), "ours content\n").unwrap();
        let ours_oid = {
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("file.txt")).unwrap();
            index.write().unwrap();
            let tree_oid = index.write_tree().unwrap();
            let tree = repo.find_tree(tree_oid).unwrap();
            let sig = crate::Signature::now("Test", "test@test.com").unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "ours change", &tree, &[&base_commit])
                .unwrap()
        };

        // Create "theirs" branch with a conflicting change
        repo.set_head(&format!(
            "refs/heads/{}",
            repo.head()
                .unwrap()
                .shorthand()
                .unwrap_or("master")
        ))
        .unwrap();
        // Go back to base
        repo.branch("theirs", &base_commit, false).unwrap();
        repo.set_head("refs/heads/theirs").unwrap();
        repo.checkout_head(Some(
            crate::build::CheckoutBuilder::new().force(),
        ))
        .unwrap();
        std::fs::write(td.path().join("file.txt"), "theirs content\n").unwrap();
        let theirs_oid = {
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("file.txt")).unwrap();
            index.write().unwrap();
            let tree_oid = index.write_tree().unwrap();
            let tree = repo.find_tree(tree_oid).unwrap();
            let sig = crate::Signature::now("Test", "test@test.com").unwrap();
            repo.commit(
                Some("HEAD"),
                &sig,
                &sig,
                "theirs change",
                &tree,
                &[&base_commit],
            )
            .unwrap()
        };

        // Now merge the two commits — this should invoke our custom driver
        let ours_commit = repo.find_commit(ours_oid).unwrap();
        let theirs_commit = repo.find_commit(theirs_oid).unwrap();
        let _merge_index = repo
            .merge_commits(&ours_commit, &theirs_commit, None)
            .unwrap();

        assert!(
            called.load(Ordering::SeqCst),
            "Custom merge driver apply should have been called"
        );

        unsafe {
            unregister_merge_driver("test-custom-invoke").unwrap();
        }
    }

    #[test]
    fn test_passthrough_result() {
        let _cfg = crate::init();
        unsafe {
            register_merge_driver("test-passthrough", NoopDriver).unwrap();
        }
        assert!(lookup_merge_driver("test-passthrough"));
        unsafe {
            unregister_merge_driver("test-passthrough").unwrap();
        }
    }
}
