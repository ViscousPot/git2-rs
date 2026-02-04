use std::any::Any;
use std::ffi::{CStr, CString};
use std::io::{self, Write};
use std::marker::PhantomData;
use std::mem;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::ptr;

use crate::util::{Binding, IntoCString};
use crate::{panic, raw, Blob, Buf, Error, Oid, Repository};

use bitflags::bitflags;
use libc::{c_char, c_int, c_void, size_t};

/// Direction of filtering operation.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FilterMode {
    /// Smudge: ODB → filesystem (checkout)
    ToWorktree,
    /// Clean: filesystem → ODB (commit)
    ToOdb,
}

impl FilterMode {
    /// Create a FilterMode from the raw libgit2 value.
    pub fn from_raw(raw: raw::git_filter_mode_t) -> FilterMode {
        match raw {
            raw::GIT_FILTER_TO_WORKTREE => FilterMode::ToWorktree,
            raw::GIT_FILTER_TO_ODB => FilterMode::ToOdb,
            _ => FilterMode::ToWorktree,
        }
    }

    fn raw(&self) -> raw::git_filter_mode_t {
        match *self {
            FilterMode::ToWorktree => raw::GIT_FILTER_TO_WORKTREE,
            FilterMode::ToOdb => raw::GIT_FILTER_TO_ODB,
        }
    }
}

bitflags! {
    /// Flags for filter list loading.
    #[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord, Hash)]
    pub struct FilterFlags: u32 {
        /// Default behavior.
        const DEFAULT = raw::GIT_FILTER_DEFAULT;
        /// Allow unsafe filters (e.g., filters that execute external commands).
        const ALLOW_UNSAFE = raw::GIT_FILTER_ALLOW_UNSAFE;
        /// Do not use system gitattributes file.
        const NO_SYSTEM_ATTRIBUTES = raw::GIT_FILTER_NO_SYSTEM_ATTRIBUTES;
        /// Get attributes from HEAD.
        const ATTRIBUTES_FROM_HEAD = raw::GIT_FILTER_ATTRIBUTES_FROM_HEAD;
        /// Get attributes from the specified commit (via options).
        const ATTRIBUTES_FROM_COMMIT = raw::GIT_FILTER_ATTRIBUTES_FROM_COMMIT;
    }
}

impl Default for FilterFlags {
    fn default() -> Self {
        FilterFlags::DEFAULT
    }
}

/// Options for extended filter list loading.
pub struct FilterOptions {
    raw: raw::git_filter_options,
}

impl FilterOptions {
    /// Create a new set of filter options with default values.
    pub fn new() -> FilterOptions {
        let mut raw: raw::git_filter_options = unsafe { mem::zeroed() };
        raw.version = raw::GIT_FILTER_OPTIONS_VERSION;
        FilterOptions { raw }
    }

    /// Set flags for filter loading.
    pub fn flags(&mut self, flags: FilterFlags) -> &mut Self {
        self.raw.flags = flags.bits();
        self
    }

    /// Set the commit from which to read attributes.
    ///
    /// Only used when ATTRIBUTES_FROM_COMMIT flag is set.
    pub fn attr_commit(&mut self, oid: Oid) -> &mut Self {
        self.raw.attr_commit_id = unsafe { *oid.raw() };
        self
    }

    fn raw_mut(&mut self) -> *mut raw::git_filter_options {
        &mut self.raw
    }
}

impl Default for FilterOptions {
    fn default() -> Self {
        FilterOptions::new()
    }
}

/// Collects filtered data into a Vec.
#[repr(C)]
struct WriteStreamCollector {
    stream: raw::git_writestream,
    data: Vec<u8>,
}

impl WriteStreamCollector {
    fn new() -> Box<WriteStreamCollector> {
        Box::new(WriteStreamCollector {
            stream: raw::git_writestream {
                write: Some(Self::write_callback),
                close: Some(Self::close_callback),
                free: Some(Self::free_callback),
            },
            data: Vec::new(),
        })
    }

    fn as_raw(&mut self) -> *mut raw::git_writestream {
        &mut self.stream
    }

    fn into_data(self) -> Vec<u8> {
        self.data
    }

    extern "C" fn write_callback(
        stream: *mut raw::git_writestream,
        buffer: *const c_char,
        len: size_t,
    ) -> c_int {
        unsafe {
            let collector = &mut *(stream as *mut WriteStreamCollector);
            let slice = std::slice::from_raw_parts(buffer as *const u8, len);
            collector.data.extend_from_slice(slice);
            0
        }
    }

    extern "C" fn close_callback(_stream: *mut raw::git_writestream) -> c_int {
        0
    }

    extern "C" fn free_callback(_stream: *mut raw::git_writestream) {
        // Memory is managed by Rust's Box, so nothing to do here
    }
}

/// Adapter for writing filtered data to a `Write` implementation.
#[repr(C)]
struct WriteStreamAdapter<'a, W: Write> {
    stream: raw::git_writestream,
    writer: &'a mut W,
    error: Option<io::Error>,
}

impl<'a, W: Write> WriteStreamAdapter<'a, W> {
    fn new(writer: &'a mut W) -> WriteStreamAdapter<'a, W> {
        WriteStreamAdapter {
            stream: raw::git_writestream {
                write: Some(Self::write_callback::<W>),
                close: Some(Self::close_callback::<W>),
                free: Some(Self::free_callback::<W>),
            },
            writer,
            error: None,
        }
    }

    fn as_raw(&mut self) -> *mut raw::git_writestream {
        &mut self.stream
    }

    fn finish(self) -> Result<(), io::Error> {
        match self.error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    extern "C" fn write_callback<WW: Write>(
        stream: *mut raw::git_writestream,
        buffer: *const c_char,
        len: size_t,
    ) -> c_int {
        unsafe {
            let adapter = &mut *(stream as *mut WriteStreamAdapter<'_, WW>);
            let slice = std::slice::from_raw_parts(buffer as *const u8, len);
            match adapter.writer.write_all(slice) {
                Ok(()) => 0,
                Err(e) => {
                    adapter.error = Some(e);
                    -1
                }
            }
        }
    }

    extern "C" fn close_callback<WW: Write>(stream: *mut raw::git_writestream) -> c_int {
        unsafe {
            let adapter = &mut *(stream as *mut WriteStreamAdapter<'_, WW>);
            match adapter.writer.flush() {
                Ok(()) => 0,
                Err(e) => {
                    adapter.error = Some(e);
                    -1
                }
            }
        }
    }

    extern "C" fn free_callback<WW: Write>(_stream: *mut raw::git_writestream) {
        // Memory is managed by the caller, nothing to do here
    }
}

/// A list of filters that should be applied to a file.
///
/// A FilterList represents the sequence of filters that need to be applied
/// to transform a file in a specific direction (to worktree or to ODB).
pub struct FilterList<'repo> {
    raw: *mut raw::git_filter_list,
    _marker: PhantomData<&'repo Repository>,
}

impl<'repo> FilterList<'repo> {
    /// Load the filters that should be applied to a file.
    ///
    /// Returns `Ok(None)` if no filters are configured for the given path.
    pub fn load(
        repo: &'repo Repository,
        blob: Option<&Blob<'_>>,
        path: &str,
        mode: FilterMode,
        flags: FilterFlags,
    ) -> Result<Option<FilterList<'repo>>, Error> {
        crate::init();
        let path = path.into_c_string()?;
        let mut raw = ptr::null_mut();
        let blob_ptr = blob.map(|b| b.raw()).unwrap_or(ptr::null_mut());
        unsafe {
            try_call!(raw::git_filter_list_load(
                &mut raw,
                repo.raw(),
                blob_ptr,
                path.as_ptr(),
                mode.raw(),
                flags.bits()
            ));
            if raw.is_null() {
                Ok(None)
            } else {
                Ok(Some(FilterList {
                    raw,
                    _marker: PhantomData,
                }))
            }
        }
    }

    /// Load the filters with extended options.
    ///
    /// Returns `Ok(None)` if no filters are configured for the given path.
    pub fn load_ext(
        repo: &'repo Repository,
        blob: Option<&Blob<'_>>,
        path: &str,
        mode: FilterMode,
        opts: &mut FilterOptions,
    ) -> Result<Option<FilterList<'repo>>, Error> {
        crate::init();
        let path = path.into_c_string()?;
        let mut raw = ptr::null_mut();
        let blob_ptr = blob.map(|b| b.raw()).unwrap_or(ptr::null_mut());
        unsafe {
            try_call!(raw::git_filter_list_load_ext(
                &mut raw,
                repo.raw(),
                blob_ptr,
                path.as_ptr(),
                mode.raw(),
                opts.raw_mut()
            ));
            if raw.is_null() {
                Ok(None)
            } else {
                Ok(Some(FilterList {
                    raw,
                    _marker: PhantomData,
                }))
            }
        }
    }

    /// Check if a filter with the given name is in this filter list.
    ///
    /// Common filter names include "crlf" and "ident".
    ///
    /// Returns an error if the filter name contains invalid UTF-8 or
    /// interior null bytes.
    pub fn contains(&self, name: &str) -> Result<bool, Error> {
        let name = name.into_c_string()?;
        Ok(unsafe { raw::git_filter_list_contains(self.raw, name.as_ptr()) != 0 })
    }

    /// Get the number of filters in this filter list.
    pub fn len(&self) -> usize {
        unsafe { raw::git_filter_list_length(self.raw) as usize }
    }

    /// Check if this filter list is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Apply the filter list to a buffer.
    pub fn apply_to_buffer(&self, input: &[u8]) -> Result<Buf, Error> {
        let buf = Buf::new();
        unsafe {
            try_call!(raw::git_filter_list_apply_to_buffer(
                buf.raw(),
                self.raw,
                input.as_ptr() as *const c_char,
                input.len() as size_t
            ));
        }
        Ok(buf)
    }

    /// Apply the filter list to a file on disk.
    pub fn apply_to_file(&self, repo: &Repository, path: &Path) -> Result<Buf, Error> {
        let path = path.into_c_string()?;
        let buf = Buf::new();
        unsafe {
            try_call!(raw::git_filter_list_apply_to_file(
                buf.raw(),
                self.raw,
                repo.raw(),
                path.as_ptr()
            ));
        }
        Ok(buf)
    }

    /// Apply the filter list to a blob.
    pub fn apply_to_blob(&self, blob: &Blob<'_>) -> Result<Buf, Error> {
        let buf = Buf::new();
        unsafe {
            try_call!(raw::git_filter_list_apply_to_blob(
                buf.raw(),
                self.raw,
                blob.raw()
            ));
        }
        Ok(buf)
    }

    /// Stream the filter list applied to a buffer, collecting the result.
    pub fn stream_buffer(&self, input: &[u8]) -> Result<Vec<u8>, Error> {
        let mut collector = WriteStreamCollector::new();
        unsafe {
            try_call!(raw::git_filter_list_stream_buffer(
                self.raw,
                input.as_ptr() as *const c_char,
                input.len() as size_t,
                collector.as_raw()
            ));
        }
        Ok(collector.into_data())
    }

    /// Stream the filter list applied to a buffer into a writer.
    pub fn stream_buffer_to_writer<W: Write>(
        &self,
        input: &[u8],
        writer: &mut W,
    ) -> Result<(), Error> {
        let mut adapter = WriteStreamAdapter::new(writer);
        unsafe {
            try_call!(raw::git_filter_list_stream_buffer(
                self.raw,
                input.as_ptr() as *const c_char,
                input.len() as size_t,
                adapter.as_raw()
            ));
        }
        adapter
            .finish()
            .map_err(|e| Error::from_str(&e.to_string()))
    }

    /// Stream the filter list applied to a file, collecting the result.
    ///
    /// The path should be relative to the repository working directory.
    pub fn stream_file(&self, repo: &Repository, path: &Path) -> Result<Vec<u8>, Error> {
        let path = path.into_c_string()?;
        let mut collector = WriteStreamCollector::new();
        unsafe {
            try_call!(raw::git_filter_list_stream_file(
                self.raw,
                repo.raw(),
                path.as_ptr(),
                collector.as_raw()
            ));
        }
        Ok(collector.into_data())
    }

    /// Stream the filter list applied to a file into a writer.
    ///
    /// The path should be relative to the repository working directory.
    pub fn stream_file_to_writer<W: Write>(
        &self,
        repo: &Repository,
        path: &Path,
        writer: &mut W,
    ) -> Result<(), Error> {
        let path = path.into_c_string()?;
        let mut adapter = WriteStreamAdapter::new(writer);
        unsafe {
            try_call!(raw::git_filter_list_stream_file(
                self.raw,
                repo.raw(),
                path.as_ptr(),
                adapter.as_raw()
            ));
        }
        adapter
            .finish()
            .map_err(|e| Error::from_str(&e.to_string()))
    }

    /// Stream the filter list applied to a blob, collecting the result.
    pub fn stream_blob(&self, blob: &Blob<'_>) -> Result<Vec<u8>, Error> {
        let mut collector = WriteStreamCollector::new();
        unsafe {
            try_call!(raw::git_filter_list_stream_blob(
                self.raw,
                blob.raw(),
                collector.as_raw()
            ));
        }
        Ok(collector.into_data())
    }

    /// Stream the filter list applied to a blob into a writer.
    pub fn stream_blob_to_writer<W: Write>(
        &self,
        blob: &Blob<'_>,
        writer: &mut W,
    ) -> Result<(), Error> {
        let mut adapter = WriteStreamAdapter::new(writer);
        unsafe {
            try_call!(raw::git_filter_list_stream_blob(
                self.raw,
                blob.raw(),
                adapter.as_raw()
            ));
        }
        adapter
            .finish()
            .map_err(|e| Error::from_str(&e.to_string()))
    }
}

unsafe impl<'repo> Send for FilterList<'repo> {}
unsafe impl<'repo> Sync for FilterList<'repo> {}

impl<'repo> Drop for FilterList<'repo> {
    fn drop(&mut self) {
        unsafe {
            raw::git_filter_list_free(self.raw);
        }
    }
}

impl<'repo> Binding for FilterList<'repo> {
    type Raw = *mut raw::git_filter_list;

    unsafe fn from_raw(raw: *mut raw::git_filter_list) -> FilterList<'repo> {
        FilterList {
            raw,
            _marker: PhantomData,
        }
    }

    fn raw(&self) -> *mut raw::git_filter_list {
        self.raw
    }
}

/// Information about the file being filtered.
pub struct FilterSource<'a> {
    raw: *const raw::git_filter_source,
    _marker: PhantomData<&'a ()>,
}

impl<'a> FilterSource<'a> {
    /// Get the path of the file being filtered.
    pub fn path(&self) -> Option<&Path> {
        unsafe {
            let ptr = raw::git_filter_source_path(self.raw);
            if ptr.is_null() {
                return None;
            }
            let path_bytes = CStr::from_ptr(ptr).to_bytes();
            Some(crate::util::bytes2path(path_bytes))
        }
    }

    /// Get the file mode of the source file.
    pub fn filemode(&self) -> u16 {
        unsafe { raw::git_filter_source_filemode(self.raw) }
    }

    /// Get the OID of the source blob (if available).
    ///
    /// This is available when filtering from ODB to worktree.
    pub fn id(&self) -> Option<Oid> {
        unsafe {
            let ptr = raw::git_filter_source_id(self.raw);
            if ptr.is_null() {
                return None;
            }
            Some(Binding::from_raw(ptr))
        }
    }

    /// Get the filtering mode (to worktree or to ODB).
    pub fn mode(&self) -> FilterMode {
        unsafe { FilterMode::from_raw(raw::git_filter_source_mode(self.raw)) }
    }

    /// Get the filter flags.
    pub fn flags(&self) -> FilterFlags {
        unsafe { FilterFlags::from_bits_truncate(raw::git_filter_source_flags(self.raw)) }
    }
}

impl<'a> Binding for FilterSource<'a> {
    type Raw = *const raw::git_filter_source;

    unsafe fn from_raw(raw: *const raw::git_filter_source) -> FilterSource<'a> {
        FilterSource {
            raw,
            _marker: PhantomData,
        }
    }

    fn raw(&self) -> *const raw::git_filter_source {
        self.raw
    }
}

/// Filter priority constants for filter registration.
///
/// These constants define the relative ordering of filters during processing.
/// Filters with lower priority values run first when going to ODB (clean),
/// and filters with higher priority values run first when going to worktree (smudge).
pub mod filter_priority {
    /// Priority of the built-in CRLF filter.
    pub const CRLF: i32 = super::raw::GIT_FILTER_CRLF_PRIORITY;
    /// Priority of the built-in ident filter.
    pub const IDENT: i32 = super::raw::GIT_FILTER_IDENT_PRIORITY;
    /// Priority of filter drivers (configured in .gitattributes).
    pub const DRIVER: i32 = super::raw::GIT_FILTER_DRIVER_PRIORITY;
}

/// Trait for implementing custom filters.
///
/// The filter registry in libgit2 is not thread-safe. Filters must be registered
/// during application startup, before any concurrent git operations.
pub trait Filter: Send + Sync + 'static {
    /// Whitespace-separated list of attribute names this filter responds to.
    ///
    /// Returns a string like `"myfilter"` or `"filter1 filter2"` for multiple attributes.
    fn attributes(&self) -> &str;

    /// Called once when the filter is first used.
    ///
    /// Override this to perform one-time initialization.
    fn initialize(&mut self) -> Result<(), Error> {
        Ok(())
    }

    /// Called when the filter is unregistered.
    ///
    /// Override this to perform cleanup.
    fn shutdown(&mut self) {}

    /// Check if this filter should be applied to the given source.
    ///
    /// Return `Ok(Some(payload))` to apply the filter (payload passed to stream),
    /// `Ok(None)` to skip (passthrough), or `Err` to abort.
    ///
    /// The `attr_values` slice contains the values of the attributes requested
    /// by `attributes()`, in the same order.
    fn check(
        &mut self,
        source: &FilterSource<'_>,
        attr_values: &[&CStr],
    ) -> Result<Option<Box<dyn Any + Send>>, Error>;

    /// Create a streaming filter for the given source.
    ///
    /// The `next` parameter is the downstream write stream. Your filter should
    /// write transformed data to `next`.
    ///
    /// The returned `FilterStream` will receive writes of the input data and
    /// should transform and write to `next`.
    fn stream(
        &mut self,
        source: &FilterSource<'_>,
        payload: &mut Option<Box<dyn Any + Send>>,
        next: &mut dyn Write,
    ) -> Result<Box<dyn FilterStream>, Error>;

    /// Called after filtering is complete to clean up the payload.
    fn cleanup(&mut self, _payload: Option<Box<dyn Any + Send>>) {}
}

/// A streaming filter output.
///
/// This trait represents a write stream that transforms data during filtering.
/// The default implementation buffers all data and writes it to `next` on close.
/// Override `write_with_next` and `close_with_next` for streaming transformations.
pub trait FilterStream: Send {
    /// Called when all input has been written; flush remaining output to `next`.
    ///
    /// The default implementation does nothing (suitable if you wrote everything
    /// in `write_with_next`).
    fn close_with_next(&mut self, _next: &mut dyn Write) -> Result<(), Error> {
        Ok(())
    }

    /// Process input data and write transformed output to `next`.
    ///
    /// This is called for each chunk of input data. Transform the data as needed
    /// and write to `next`.
    fn write_with_next(&mut self, buf: &[u8], next: &mut dyn Write) -> io::Result<usize>;
}

/// Instance of a `git_filter`, must use `#[repr(C)]` to ensure that
/// the C fields come first.
#[repr(C)]
struct RawFilter {
    raw: raw::git_filter,
    obj: Box<dyn Filter>,
    initialized: bool,
    /// Owned attributes string - must be kept alive for the lifetime of the filter
    /// since raw.attributes points to it.
    _attributes: CString,
}

/// Instance of a `git_writestream` for custom filters.
#[repr(C)]
struct RawFilterStream {
    stream: raw::git_writestream,
    filter_stream: Box<dyn FilterStream>,
    next: *mut raw::git_writestream,
    /// Stores the last IO error that occurred during filtering.
    /// This allows us to preserve error information when returning -1 to libgit2.
    last_error: Option<io::Error>,
}

/// Register a custom filter.
///
/// # Safety
///
/// This function is unsafe because the libgit2 filter registry is not thread-safe.
/// You must ensure that:
/// 1. No concurrent git operations are in progress
/// 2. Registration happens during application initialization
///
/// # Arguments
/// * `name` - Name to register the filter under (e.g., "myfilter")
/// * `filter` - The filter implementation
/// * `priority` - Filter priority (use constants from `filter_priority` module)
///
/// # Example
///
/// ```ignore
/// unsafe {
///     git2::register_filter("myfilter", MyFilter::new(), git2::filter_priority::DRIVER)?;
/// }
/// ```
pub unsafe fn register_filter<F: Filter>(
    name: &str,
    filter: F,
    priority: i32,
) -> Result<(), Error> {
    crate::init();

    let name = CString::new(name)?;
    let attributes = CString::new(filter.attributes())?;

    // Create the RawFilter with the attributes stored inside.
    // The raw.attributes pointer points to the _attributes field,
    // which remains valid for the lifetime of RawFilter.
    let mut raw_filter = Box::new(RawFilter {
        raw: raw::git_filter {
            version: raw::GIT_FILTER_VERSION,
            attributes: attributes.as_ptr(),
            initialize: Some(filter_init_cb),
            shutdown: Some(filter_shutdown_cb),
            check: Some(filter_check_cb),
            apply: ptr::null_mut(),
            stream: Some(filter_stream_cb),
            cleanup: Some(filter_cleanup_cb),
        },
        obj: Box::new(filter),
        initialized: false,
        _attributes: attributes,
    });

    // Update the pointer to point to the stored CString
    raw_filter.raw.attributes = raw_filter._attributes.as_ptr();

    let rc = raw::git_filter_register(
        name.as_ptr(),
        &mut raw_filter.raw as *mut raw::git_filter,
        priority,
    );

    if rc < 0 {
        return Err(Error::last_error(rc));
    }

    // Transfer ownership of the filter to libgit2
    // The filter (including _attributes) will be freed in filter_shutdown_cb
    mem::forget(raw_filter);

    Ok(())
}

/// Unregister a custom filter.
///
/// # Safety
///
/// Same thread-safety requirements as `register_filter`.
/// This function must not be called while any git operations are in progress
/// that might use this filter.
pub unsafe fn unregister_filter(name: &str) -> Result<(), Error> {
    crate::init();
    let name = CString::new(name)?;
    let rc = raw::git_filter_unregister(name.as_ptr());
    if rc < 0 {
        return Err(Error::last_error(rc));
    }
    Ok(())
}

/// Look up a registered filter by name.
///
/// Returns `true` if a filter with the given name is registered, `false` otherwise.
pub fn lookup_filter(name: &str) -> bool {
    crate::init();
    let name = match CString::new(name) {
        Ok(s) => s,
        Err(_) => return false,
    };
    unsafe { !raw::git_filter_lookup(name.as_ptr()).is_null() }
}

extern "C" fn filter_init_cb(filter: *mut raw::git_filter) -> c_int {
    unsafe {
        let raw_filter = &mut *(filter as *mut RawFilter);
        if raw_filter.initialized {
            return 0;
        }
        match panic::wrap(AssertUnwindSafe(|| raw_filter.obj.initialize())) {
            Some(Ok(())) => {
                raw_filter.initialized = true;
                0
            }
            Some(Err(_)) => -1,
            None => -1,
        }
    }
}

extern "C" fn filter_shutdown_cb(filter: *mut raw::git_filter) {
    unsafe {
        let raw_filter = &mut *(filter as *mut RawFilter);
        let _ = panic::wrap(AssertUnwindSafe(|| raw_filter.obj.shutdown()));

        // Clean up the filter - it was allocated in register_filter
        let _ = Box::from_raw(filter as *mut RawFilter);
    }
}

extern "C" fn filter_check_cb(
    filter: *mut raw::git_filter,
    payload: *mut *mut c_void,
    src: *const raw::git_filter_source,
    attr_values: *mut *const c_char,
) -> c_int {
    unsafe {
        let raw_filter = &mut *(filter as *mut RawFilter);
        let source = FilterSource::from_raw(src);

        // Convert attribute values to a slice of CStr
        let mut attrs: Vec<&CStr> = Vec::new();
        if !attr_values.is_null() {
            let mut ptr = attr_values;
            while !(*ptr).is_null() {
                attrs.push(CStr::from_ptr(*ptr));
                ptr = ptr.add(1);
            }
        }

        match panic::wrap(AssertUnwindSafe(|| raw_filter.obj.check(&source, &attrs))) {
            Some(Ok(Some(p))) => {
                // Store payload for use in stream callback
                *payload = Box::into_raw(Box::new(p)) as *mut c_void;
                0 // Apply the filter
            }
            Some(Ok(None)) => {
                *payload = ptr::null_mut();
                raw::GIT_PASSTHROUGH // Skip this filter
            }
            Some(Err(_)) => -1,
            None => -1,
        }
    }
}

extern "C" fn filter_stream_cb(
    out: *mut *mut raw::git_writestream,
    filter: *mut raw::git_filter,
    payload: *mut *mut c_void,
    src: *const raw::git_filter_source,
    next: *mut raw::git_writestream,
) -> c_int {
    unsafe {
        let raw_filter = &mut *(filter as *mut RawFilter);
        let source = FilterSource::from_raw(src);

        // Get the payload from the check callback
        let mut user_payload: Option<Box<dyn Any + Send>> = if (*payload).is_null() {
            None
        } else {
            Some(*Box::from_raw(*payload as *mut Box<dyn Any + Send>))
        };

        // Create a wrapper for the next stream
        let mut next_writer = NextStreamWriter { next };

        match panic::wrap(AssertUnwindSafe(|| {
            raw_filter.obj.stream(&source, &mut user_payload, &mut next_writer)
        })) {
            Some(Ok(stream)) => {
                // Store the updated payload back
                if let Some(p) = user_payload {
                    *payload = Box::into_raw(Box::new(p)) as *mut c_void;
                } else {
                    *payload = ptr::null_mut();
                }

                // Create the raw stream wrapper
                let raw_stream = Box::new(RawFilterStream {
                    stream: raw::git_writestream {
                        write: Some(filter_stream_write_cb),
                        close: Some(filter_stream_close_cb),
                        free: Some(filter_stream_free_cb),
                    },
                    filter_stream: stream,
                    next,
                    last_error: None,
                });

                *out = Box::into_raw(raw_stream) as *mut raw::git_writestream;
                0
            }
            Some(Err(_)) => -1,
            None => -1,
        }
    }
}

extern "C" fn filter_cleanup_cb(filter: *mut raw::git_filter, payload: *mut c_void) {
    unsafe {
        let raw_filter = &mut *(filter as *mut RawFilter);

        let user_payload: Option<Box<dyn Any + Send>> = if payload.is_null() {
            None
        } else {
            Some(*Box::from_raw(payload as *mut Box<dyn Any + Send>))
        };

        let _ = panic::wrap(AssertUnwindSafe(|| raw_filter.obj.cleanup(user_payload)));
    }
}

// Stream callbacks
extern "C" fn filter_stream_write_cb(
    stream: *mut raw::git_writestream,
    buffer: *const c_char,
    len: size_t,
) -> c_int {
    unsafe {
        let raw_stream = &mut *(stream as *mut RawFilterStream);
        let slice = std::slice::from_raw_parts(buffer as *const u8, len);
        let mut next_writer = NextStreamWriter {
            next: raw_stream.next,
        };
        match panic::wrap(AssertUnwindSafe(|| {
            raw_stream
                .filter_stream
                .write_with_next(slice, &mut next_writer)
        })) {
            Some(Ok(_)) => 0,
            Some(Err(e)) => {
                // Store the error for potential retrieval
                raw_stream.last_error = Some(e);
                -1
            }
            None => {
                // Panic occurred
                raw_stream.last_error =
                    Some(io::Error::new(io::ErrorKind::Other, "panic in filter"));
                -1
            }
        }
    }
}

extern "C" fn filter_stream_close_cb(stream: *mut raw::git_writestream) -> c_int {
    unsafe {
        let raw_stream = &mut *(stream as *mut RawFilterStream);
        let mut next_writer = NextStreamWriter {
            next: raw_stream.next,
        };
        // First, let our filter flush any remaining data to next
        let result = panic::wrap(AssertUnwindSafe(|| {
            raw_stream
                .filter_stream
                .close_with_next(&mut next_writer)
        }));
        match result {
            Some(Ok(())) => {
                // Then close the next stream in the chain
                match next_writer.close() {
                    Ok(()) => 0,
                    Err(e) => {
                        raw_stream.last_error = Some(e);
                        -1
                    }
                }
            }
            Some(Err(e)) => {
                raw_stream.last_error =
                    Some(io::Error::new(io::ErrorKind::Other, e.message()));
                -1
            }
            None => {
                raw_stream.last_error =
                    Some(io::Error::new(io::ErrorKind::Other, "panic in filter close"));
                -1
            }
        }
    }
}

extern "C" fn filter_stream_free_cb(stream: *mut raw::git_writestream) {
    unsafe {
        // Clean up the stream
        let _ = Box::from_raw(stream as *mut RawFilterStream);
    }
}

/// Helper struct to write to the next stream in the filter chain
struct NextStreamWriter {
    next: *mut raw::git_writestream,
}

impl NextStreamWriter {
    /// Close the underlying stream.
    fn close(&mut self) -> io::Result<()> {
        unsafe {
            if let Some(close_fn) = (*self.next).close {
                let rc = close_fn(self.next);
                if rc < 0 {
                    Err(io::Error::new(io::ErrorKind::Other, "filter close failed"))
                } else {
                    Ok(())
                }
            } else {
                Ok(())
            }
        }
    }
}

impl Write for NextStreamWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        unsafe {
            if let Some(write_fn) = (*self.next).write {
                let rc = write_fn(self.next, buf.as_ptr() as *const c_char, buf.len());
                if rc < 0 {
                    Err(io::Error::new(io::ErrorKind::Other, "filter write failed"))
                } else {
                    Ok(buf.len())
                }
            } else {
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "no write function on stream",
                ))
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

unsafe impl Send for NextStreamWriter {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Repository;
    use std::fs;
    use tempfile::TempDir;

    fn make_test_repo() -> (TempDir, Repository) {
        let td = TempDir::new().unwrap();
        let repo = Repository::init(td.path()).unwrap();
        (td, repo)
    }

    #[test]
    fn test_filter_list_load_empty() {
        let (_td, repo) = make_test_repo();
        // No .gitattributes, so no filters should be loaded
        let filters =
            FilterList::load(&repo, None, "test.txt", FilterMode::ToWorktree, FilterFlags::DEFAULT)
                .unwrap();
        assert!(
            filters.is_none(),
            "Expected no filters without .gitattributes"
        );
    }

    #[test]
    fn test_filter_list_with_crlf() {
        let (td, repo) = make_test_repo();

        // Create a .gitattributes file that sets text=auto for all files
        fs::write(td.path().join(".gitattributes"), "* text=auto\n").unwrap();

        // The API should work regardless of whether filters are loaded
        let result = FilterList::load(
            &repo,
            None,
            "test.txt",
            FilterMode::ToWorktree,
            FilterFlags::DEFAULT,
        );
        assert!(result.is_ok(), "FilterList::load should not fail");
        // Note: Whether filters are actually loaded depends on git config and platform
    }

    #[test]
    fn test_filter_list_with_ident() {
        let (td, repo) = make_test_repo();

        // Create a .gitattributes file that enables ident filter
        fs::write(td.path().join(".gitattributes"), "*.txt ident\n").unwrap();

        let filters = FilterList::load(
            &repo,
            None,
            "test.txt",
            FilterMode::ToWorktree,
            FilterFlags::DEFAULT,
        )
        .unwrap();

        // The ident filter should be loaded for .txt files
        let filters = filters.expect("Expected ident filter to be loaded for *.txt pattern");
        assert!(
            filters.contains("ident").unwrap(),
            "Filter list should contain 'ident' filter"
        );
        assert!(!filters.is_empty(), "Filter list should not be empty");
    }

    #[test]
    fn test_apply_to_buffer_with_ident() {
        let (td, repo) = make_test_repo();

        // Create a .gitattributes file with ident filter
        fs::write(td.path().join(".gitattributes"), "*.txt ident\n").unwrap();

        let filters = FilterList::load(
            &repo,
            None,
            "test.txt",
            FilterMode::ToOdb,
            FilterFlags::DEFAULT,
        )
        .unwrap();

        let filters = filters.expect("Expected ident filter to be loaded");
        // When going TO_ODB, $Id: ...$ should be collapsed to $Id$
        let input = b"$Id: abc123 $\nHello World\n";
        let result = filters.apply_to_buffer(input).unwrap();

        // The ident filter should collapse $Id: ...$ to $Id$
        let content = std::str::from_utf8(&result).unwrap();
        assert!(
            content.contains("$Id$"),
            "Expected $Id$ in output, got: {}",
            content
        );
    }

    #[test]
    fn test_filter_options() {
        let (_td, repo) = make_test_repo();
        let mut opts = FilterOptions::new();
        opts.flags(FilterFlags::NO_SYSTEM_ATTRIBUTES);

        let result =
            FilterList::load_ext(&repo, None, "test.txt", FilterMode::ToWorktree, &mut opts);
        assert!(result.is_ok(), "load_ext should succeed");
    }

    #[test]
    fn test_apply_to_blob() {
        let (td, repo) = make_test_repo();

        // Create a .gitattributes file
        fs::write(td.path().join(".gitattributes"), "*.txt ident\n").unwrap();

        // Create a blob with $Id$ placeholder
        let oid = repo.blob(b"$Id$\nHello World\n").unwrap();
        let blob = repo.find_blob(oid).unwrap();

        // Load filters for a .txt file going to worktree (should expand $Id$)
        let filters = FilterList::load(
            &repo,
            Some(&blob),
            "test.txt",
            FilterMode::ToWorktree,
            FilterFlags::DEFAULT,
        )
        .unwrap();

        let filters = filters.expect("Expected ident filter to be loaded");
        let result = filters.apply_to_blob(&blob).unwrap();

        // The $Id$ should be expanded to include the blob SHA
        let content = std::str::from_utf8(&result).unwrap();
        assert!(
            content.starts_with("$Id:"),
            "Expected $Id: expansion, got: {}",
            content
        );
        // Verify the OID is included
        let oid_str = oid.to_string();
        assert!(
            content.contains(&oid_str),
            "Expected blob OID {} in expansion, got: {}",
            oid_str,
            content
        );
    }

    #[test]
    fn test_repository_filter_list_convenience() {
        let (td, repo) = make_test_repo();

        // Create a .gitattributes file
        fs::write(td.path().join(".gitattributes"), "*.txt ident\n").unwrap();

        // Use the Repository::filter_list convenience method
        let result = repo.filter_list(
            None,
            "test.txt",
            FilterMode::ToWorktree,
            FilterFlags::DEFAULT,
        );
        assert!(result.is_ok());
        assert!(
            result.unwrap().is_some(),
            "Expected filter for *.txt pattern"
        );

        // With no matching attributes, we should get None
        let filters_none = repo.filter_list(
            None,
            "test.bin",
            FilterMode::ToWorktree,
            FilterFlags::DEFAULT,
        );
        assert!(filters_none.is_ok());
        assert!(
            filters_none.unwrap().is_none(),
            "Expected no filter for .bin extension"
        );
    }

    #[test]
    fn test_filter_list_len() {
        let (td, repo) = make_test_repo();

        // Create a .gitattributes file with ident filter
        fs::write(td.path().join(".gitattributes"), "*.txt ident\n").unwrap();

        let filters = FilterList::load(
            &repo,
            None,
            "test.txt",
            FilterMode::ToWorktree,
            FilterFlags::DEFAULT,
        )
        .unwrap();

        let filters = filters.expect("Expected ident filter to be loaded");
        assert_eq!(filters.len(), 1, "Should have exactly 1 filter (ident)");
        assert!(!filters.is_empty());
    }

    #[test]
    fn test_stream_buffer_to_writer() {
        let (td, repo) = make_test_repo();

        // Create a .gitattributes file with ident filter
        fs::write(td.path().join(".gitattributes"), "*.txt ident\n").unwrap();

        let filters = FilterList::load(
            &repo,
            None,
            "test.txt",
            FilterMode::ToOdb,
            FilterFlags::DEFAULT,
        )
        .unwrap();

        let filters = filters.expect("Expected ident filter to be loaded");

        // Test streaming to a Vec<u8> writer
        let input = b"$Id: abc123 $\nContent\n";
        let mut output = Vec::new();
        filters.stream_buffer_to_writer(input, &mut output).unwrap();

        let content = String::from_utf8(output).unwrap();
        assert!(
            content.contains("$Id$"),
            "Expected collapsed $Id$, got: {}",
            content
        );
    }

    #[test]
    fn test_lookup_builtin_filters() {
        // The crlf and ident filters should be registered by default
        assert!(super::lookup_filter("crlf"), "crlf filter should exist");
        assert!(super::lookup_filter("ident"), "ident filter should exist");
        assert!(
            !super::lookup_filter("nonexistent"),
            "nonexistent filter should not exist"
        );
    }

}
