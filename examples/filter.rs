/*
 * Custom filter example - demonstrates filter triggering during git operations.
 *
 * To the extent possible under law, the author(s) have dedicated all copyright
 * and related and neighboring rights to this software to the public domain
 * worldwide. This software is distributed without any warranty.
 *
 * You should have received a copy of the CC0 Public Domain Dedication along
 * with this software. If not, see
 * <http://creativecommons.org/publicdomain/zero/1.0/>.
 */

#![deny(warnings)]

use git2::build::CheckoutBuilder;
use git2::{
    filter_priority, register_filter, unregister_filter, Error, Filter, FilterMode, FilterSource,
    FilterStream, Repository,
};
use std::any::Any;
use std::ffi::CStr;
use std::fs;
use std::io::Write;
use std::path::Path;
use tempfile::TempDir;

/// A filter that logs smudge/clean operations.
struct PrintFilter;

impl Filter for PrintFilter {
    fn attributes(&self) -> &str {
        "printfilter"
    }

    fn check(
        &mut self,
        source: &FilterSource<'_>,
        _attr_values: &[&CStr],
    ) -> Result<Option<Box<dyn Any + Send>>, Error> {
        let is_txt = source
            .path()
            .and_then(|p| p.extension())
            .map(|ext| ext == "txt")
            .unwrap_or(false);

        if !is_txt {
            return Ok(None);
        }

        match source.mode() {
            FilterMode::ToWorktree => println!(">>> SMUDGE: {:?}", source.path()),
            FilterMode::ToOdb => println!(">>> CLEAN: {:?}", source.path()),
        }

        Ok(Some(Box::new(())))
    }

    fn stream(
        &mut self,
        _source: &FilterSource<'_>,
        _payload: &mut Option<Box<dyn Any + Send>>,
        _next: &mut dyn Write,
    ) -> Result<Box<dyn FilterStream>, Error> {
        Ok(Box::new(PassthroughStream { buffer: Vec::new() }))
    }
}

struct PassthroughStream {
    buffer: Vec<u8>,
}

impl FilterStream for PassthroughStream {
    fn write_with_next(&mut self, buf: &[u8], _next: &mut dyn Write) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn close_with_next(&mut self, next: &mut dyn Write) -> Result<(), Error> {
        next.write_all(&self.buffer)
            .map_err(|e| Error::from_str(&e.to_string()))?;
        Ok(())
    }
}

// SAFETY: PassthroughStream contains only owned data (Vec<u8>)
unsafe impl Send for PassthroughStream {}

fn run() -> Result<(), Error> {
    unsafe {
        register_filter("printfilter", PrintFilter, filter_priority::DRIVER)?;
    }
    println!("Registered 'printfilter' custom filter");

    let temp_dir = TempDir::new().map_err(|e| Error::from_str(&e.to_string()))?;
    let repo = Repository::init(temp_dir.path())?;

    fs::write(temp_dir.path().join(".gitattributes"), "*.txt filter=printfilter\n")
        .map_err(|e| Error::from_str(&e.to_string()))?;

    let mut index = repo.index()?;
    index.add_path(Path::new(".gitattributes"))?;
    index.write()?;

    let test_file_path = temp_dir.path().join("test.txt");
    fs::write(&test_file_path, "Hello World\n").map_err(|e| Error::from_str(&e.to_string()))?;

    println!("--- Staging test.txt (should trigger CLEAN) ---");
    index.add_path(Path::new("test.txt"))?;
    index.write()?;

    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;
    let sig = repo.signature().unwrap_or_else(|_| {
        git2::Signature::now("Test User", "test@example.com").unwrap()
    });
    repo.commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])?;

    fs::write(&test_file_path, "Modified content\n")
        .map_err(|e| Error::from_str(&e.to_string()))?;

    println!("--- Checking out HEAD (should trigger SMUDGE) ---");
    let mut checkout_opts = CheckoutBuilder::new();
    checkout_opts.force();
    repo.checkout_head(Some(&mut checkout_opts))?;

    unsafe {
        unregister_filter("printfilter")?;
    }

    println!("=== Filter test completed successfully! ===");
    Ok(())
}

fn main() {
    match run() {
        Ok(()) => {}
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    }
}
