use std::io;
use std::marker;
use std::mem;
use std::path::Path;
use std::ptr;
use std::slice;

use bitflags::bitflags;

use crate::util::Binding;
use crate::{raw, Buf, Error, Object, Oid};

bitflags! {
    /// Flags to control blob filtering behavior.
    #[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord, Hash)]
    pub struct BlobFilterFlags: u32 {
        /// When set, filters will not be applied to binary files.
        const CHECK_FOR_BINARY = raw::GIT_BLOB_FILTER_CHECK_FOR_BINARY as u32;
        /// Don't load `/etc/gitattributes` (or system equivalent).
        const NO_SYSTEM_ATTRIBUTES = raw::GIT_BLOB_FILTER_NO_SYSTEM_ATTRIBUTES as u32;
        /// Load attributes from `.gitattributes` in root of HEAD.
        const ATTRIBUTES_FROM_HEAD = raw::GIT_BLOB_FILTER_ATTRIBUTES_FROM_HEAD as u32;
        /// Load attributes from a specific commit.
        const ATTRIBUTES_FROM_COMMIT = raw::GIT_BLOB_FILTER_ATTRIBUTES_FROM_COMMIT as u32;
    }
}

impl Default for BlobFilterFlags {
    fn default() -> Self {
        BlobFilterFlags::CHECK_FOR_BINARY
    }
}

/// Options for filtering a blob.
pub struct BlobFilterOptions {
    raw: raw::git_blob_filter_options,
}

impl BlobFilterOptions {
    /// Create a new set of blob filter options with default values.
    pub fn new() -> BlobFilterOptions {
        let mut raw = unsafe { mem::zeroed() };
        unsafe {
            raw::git_blob_filter_options_init(&mut raw, raw::GIT_BLOB_FILTER_OPTIONS_VERSION);
        }
        BlobFilterOptions { raw }
    }

    /// Set flags for blob filtering.
    pub fn flags(&mut self, flags: BlobFilterFlags) -> &mut Self {
        self.raw.flags = flags.bits();
        self
    }

    /// Set the commit from which to read attributes.
    ///
    /// Only used when ATTRIBUTES_FROM_COMMIT flag is set.
    pub fn attr_commit(&mut self, commit: Oid) -> &mut Self {
        self.raw.attr_commit_id = unsafe { *commit.raw() };
        self
    }
}

impl Default for BlobFilterOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// A structure to represent a git [blob][1]
///
/// [1]: http://git-scm.com/book/en/Git-Internals-Git-Objects
pub struct Blob<'repo> {
    raw: *mut raw::git_blob,
    _marker: marker::PhantomData<Object<'repo>>,
}

impl<'repo> Blob<'repo> {
    /// Get the id (SHA1) of a repository blob
    pub fn id(&self) -> Oid {
        unsafe { Binding::from_raw(raw::git_blob_id(&*self.raw)) }
    }

    /// Determine if the blob content is most certainly binary or not.
    pub fn is_binary(&self) -> bool {
        unsafe { raw::git_blob_is_binary(&*self.raw) == 1 }
    }

    /// Get the content of this blob.
    pub fn content(&self) -> &[u8] {
        unsafe {
            let data = raw::git_blob_rawcontent(&*self.raw) as *const u8;
            let len = raw::git_blob_rawsize(&*self.raw) as usize;
            slice::from_raw_parts(data, len)
        }
    }

    /// Get the size in bytes of the contents of this blob.
    pub fn size(&self) -> usize {
        unsafe { raw::git_blob_rawsize(&*self.raw) as usize }
    }

    /// Casts this Blob to be usable as an `Object`
    pub fn as_object(&self) -> &Object<'repo> {
        unsafe { &*(self as *const _ as *const Object<'repo>) }
    }

    /// Consumes Blob to be returned as an `Object`
    pub fn into_object(self) -> Object<'repo> {
        assert_eq!(mem::size_of_val(&self), mem::size_of::<Object<'_>>());
        unsafe { mem::transmute(self) }
    }

    /// Filter blob content through the configured filters.
    ///
    /// This applies gitattributes filters (crlf, ident, etc.) to the blob content
    /// as if it were being checked out to the given path.
    ///
    /// # Arguments
    /// * `as_path` - Path to use for attribute lookups
    /// * `opts` - Optional filtering options
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use git2::Repository;
    ///
    /// let repo = Repository::open("/path/to/repo")?;
    /// let blob = repo.find_blob(repo.head()?.peel_to_blob()?.id())?;
    /// let filtered = blob.filter("file.txt", None)?;
    /// # Ok::<(), git2::Error>(())
    /// ```
    pub fn filter<P: AsRef<Path>>(
        &self,
        as_path: P,
        opts: Option<&mut BlobFilterOptions>,
    ) -> Result<Buf, Error> {
        let as_path = crate::util::cstring_to_repo_path(as_path.as_ref())?;
        let buf = Buf::new();
        unsafe {
            let opts_ptr = opts
                .map(|o| &mut o.raw as *mut _)
                .unwrap_or(ptr::null_mut());
            try_call!(raw::git_blob_filter(
                buf.raw(),
                self.raw,
                as_path.as_ptr(),
                opts_ptr
            ));
        }
        Ok(buf)
    }
}

impl<'repo> Binding for Blob<'repo> {
    type Raw = *mut raw::git_blob;

    unsafe fn from_raw(raw: *mut raw::git_blob) -> Blob<'repo> {
        Blob {
            raw,
            _marker: marker::PhantomData,
        }
    }
    fn raw(&self) -> *mut raw::git_blob {
        self.raw
    }
}

impl<'repo> std::fmt::Debug for Blob<'repo> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.debug_struct("Blob").field("id", &self.id()).finish()
    }
}

impl<'repo> Clone for Blob<'repo> {
    fn clone(&self) -> Self {
        self.as_object().clone().into_blob().ok().unwrap()
    }
}

impl<'repo> Drop for Blob<'repo> {
    fn drop(&mut self) {
        unsafe { raw::git_blob_free(self.raw) }
    }
}

/// A structure to represent a git writestream for blobs
pub struct BlobWriter<'repo> {
    raw: *mut raw::git_writestream,
    need_cleanup: bool,
    _marker: marker::PhantomData<Object<'repo>>,
}

impl<'repo> BlobWriter<'repo> {
    /// Finalize blob writing stream and write the blob to the object db
    pub fn commit(mut self) -> Result<Oid, Error> {
        // After commit we already doesn't need cleanup on drop
        self.need_cleanup = false;
        let mut raw = crate::util::zeroed_raw_oid();
        unsafe {
            try_call!(raw::git_blob_create_fromstream_commit(&mut raw, self.raw));
            Ok(Binding::from_raw(&raw as *const _))
        }
    }
}

impl<'repo> Binding for BlobWriter<'repo> {
    type Raw = *mut raw::git_writestream;

    unsafe fn from_raw(raw: *mut raw::git_writestream) -> BlobWriter<'repo> {
        BlobWriter {
            raw,
            need_cleanup: true,
            _marker: marker::PhantomData,
        }
    }
    fn raw(&self) -> *mut raw::git_writestream {
        self.raw
    }
}

impl<'repo> Drop for BlobWriter<'repo> {
    fn drop(&mut self) {
        // We need cleanup in case the stream has not been committed
        if self.need_cleanup {
            unsafe {
                if let Some(f) = (*self.raw).free {
                    f(self.raw)
                }
            }
        }
    }
}

impl<'repo> io::Write for BlobWriter<'repo> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        unsafe {
            if let Some(f) = (*self.raw).write {
                let res = f(self.raw, buf.as_ptr() as *const _, buf.len());
                if res < 0 {
                    Err(io::Error::new(io::ErrorKind::Other, "Write error"))
                } else {
                    Ok(buf.len())
                }
            } else {
                Err(io::Error::new(io::ErrorKind::Other, "no write callback"))
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Repository;
    use std::fs;
    use std::fs::File;
    use std::io::prelude::*;
    use std::path::Path;
    use tempfile::TempDir;

    #[test]
    fn buffer() {
        let td = TempDir::new().unwrap();
        let repo = Repository::init(td.path()).unwrap();
        let id = repo.blob(&[5, 4, 6]).unwrap();
        let blob = repo.find_blob(id).unwrap();

        assert_eq!(blob.id(), id);
        assert_eq!(blob.size(), 3);
        assert_eq!(blob.content(), [5, 4, 6]);
        assert!(blob.is_binary());

        repo.find_object(id, None).unwrap().as_blob().unwrap();
        repo.find_object(id, None)
            .unwrap()
            .into_blob()
            .ok()
            .unwrap();
    }

    #[test]
    fn path() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("foo");
        File::create(&path).unwrap().write_all(&[7, 8, 9]).unwrap();
        let repo = Repository::init(td.path()).unwrap();
        let id = repo.blob_path(&path).unwrap();
        let blob = repo.find_blob(id).unwrap();
        assert_eq!(blob.content(), [7, 8, 9]);
        blob.into_object();
    }

    #[test]
    fn stream() {
        let td = TempDir::new().unwrap();
        let repo = Repository::init(td.path()).unwrap();
        let mut ws = repo.blob_writer(Some(Path::new("foo"))).unwrap();
        let wl = ws.write(&[10, 11, 12]).unwrap();
        assert_eq!(wl, 3);
        let id = ws.commit().unwrap();
        let blob = repo.find_blob(id).unwrap();
        assert_eq!(blob.content(), [10, 11, 12]);
        blob.into_object();
    }

    #[test]
    fn test_blob_filter_default() {
        let td = TempDir::new().unwrap();
        let repo = Repository::init(td.path()).unwrap();

        // Create a blob with simple content
        let id = repo.blob(b"Hello World\n").unwrap();
        let blob = repo.find_blob(id).unwrap();

        // Filter without any .gitattributes (should return content unchanged)
        let filtered = blob.filter("test.txt", None).unwrap();
        assert_eq!(filtered.as_ref(), b"Hello World\n");
    }

    #[test]
    fn test_blob_filter_with_ident() {
        let td = TempDir::new().unwrap();
        let repo = Repository::init(td.path()).unwrap();

        // Create a .gitattributes file with ident filter
        fs::write(td.path().join(".gitattributes"), "*.txt ident\n").unwrap();

        // Create a blob with $Id$ placeholder
        let id = repo.blob(b"$Id$\nHello World\n").unwrap();
        let blob = repo.find_blob(id).unwrap();

        // Filter the blob - should expand $Id$ to include the blob SHA
        let filtered = blob.filter("test.txt", None).unwrap();
        let content = std::str::from_utf8(&filtered).unwrap();

        // The $Id$ should be expanded to include the blob SHA
        assert!(
            content.starts_with("$Id:"),
            "Expected $Id: expansion, got: {}",
            content
        );
        let oid_str = id.to_string();
        assert!(
            content.contains(&oid_str),
            "Expected blob OID {} in expansion, got: {}",
            oid_str,
            content
        );
    }

    #[test]
    fn test_blob_filter_with_options() {
        let td = TempDir::new().unwrap();
        let repo = Repository::init(td.path()).unwrap();

        // Create a .gitattributes file with ident filter
        fs::write(td.path().join(".gitattributes"), "*.txt ident\n").unwrap();

        // Create a blob
        let id = repo.blob(b"$Id$\nContent\n").unwrap();
        let blob = repo.find_blob(id).unwrap();

        // Filter with options (not using ATTRIBUTES_FROM_HEAD since HEAD doesn't exist)
        let mut opts = BlobFilterOptions::new();
        opts.flags(BlobFilterFlags::CHECK_FOR_BINARY | BlobFilterFlags::NO_SYSTEM_ATTRIBUTES);

        let filtered = blob.filter("test.txt", Some(&mut opts)).unwrap();
        // Ident filter should expand $Id$ even with custom options
        assert!(!filtered.is_empty());
        let content = std::str::from_utf8(&filtered).unwrap();
        assert!(
            content.contains("$Id"),
            "Expected ident expansion in output, got: {}",
            content
        );
    }

    #[test]
    fn test_blob_filter_flags_default() {
        // Default flags should include CHECK_FOR_BINARY to skip binary files
        let flags = BlobFilterFlags::default();
        assert!(flags.contains(BlobFilterFlags::CHECK_FOR_BINARY));
    }
}
