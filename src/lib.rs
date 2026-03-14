//! Shared utilities for nix2flatpak tools.
//!
//! Provides ELF inspection helpers and Nix store path manipulation
//! used across the analyze, index, and rewrite binaries.

use std::fs;
use std::io::Read;
use std::path::Path;

use goblin::elf::Elf;

/// Check if a file starts with the ELF magic bytes.
pub fn is_elf(path: &Path) -> bool {
    let Ok(mut f) = fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic).is_ok() && magic == *b"\x7fELF"
}

/// Extract the SONAME from an ELF shared library, if present.
pub fn extract_soname(path: &Path) -> Option<String> {
    let data = fs::read(path).ok()?;
    extract_soname_from_bytes(&data)
}

/// Extract the SONAME from already-loaded ELF bytes.
pub fn extract_soname_from_bytes(data: &[u8]) -> Option<String> {
    if !data.starts_with(b"\x7fELF") {
        return None;
    }
    let elf = Elf::parse(data).ok()?;
    elf.soname.map(|s| s.to_string())
}

/// Extract all DT_NEEDED library names from an ELF binary.
pub fn extract_needed(data: &[u8]) -> Vec<String> {
    if !data.starts_with(b"\x7fELF") {
        return Vec::new();
    }
    match Elf::parse(data) {
        Ok(elf) => elf.libraries.iter().map(|s| s.to_string()).collect(),
        Err(_) => Vec::new(),
    }
}

/// Check if a file is a text file (no null bytes in first 8 KiB).
pub fn is_text_file(path: &Path) -> bool {
    let Ok(mut f) = fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 8192];
    let n = match f.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return false,
    };
    !buf[..n].contains(&0)
}

/// Check if a file starts with a `#!` shebang.
pub fn is_script(path: &Path) -> bool {
    let Ok(mut f) = fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 2];
    f.read_exact(&mut magic).is_ok() && magic == *b"#!"
}

/// Extract the 32-character hash from a Nix store path.
///
/// `/nix/store/abc123…-name` → `abc123…`
pub fn store_path_hash(store_path: &str) -> &str {
    let basename = store_path.rsplit('/').next().unwrap_or(store_path);
    match basename.find('-') {
        Some(idx) => &basename[..idx],
        None => basename,
    }
}

/// Copy a directory tree, preserving symlinks.
pub fn copy_tree(src: &Path, dst: &Path) -> anyhow::Result<()> {
    use anyhow::Context;
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src).with_context(|| format!("reading dir {}", src.display()))? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let sym_meta = src_path
            .symlink_metadata()
            .with_context(|| format!("stat {}", src_path.display()))?;

        if sym_meta.file_type().is_symlink() {
            let target = fs::read_link(&src_path)?;
            std::os::unix::fs::symlink(&target, &dst_path)?;
        } else if sym_meta.is_dir() {
            copy_tree(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)
                .with_context(|| format!("copying {} -> {}", src_path.display(), dst_path.display()))?;
        }
    }
    Ok(())
}

/// Make a file writable by its owner.
pub fn make_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = path.metadata() {
        let mode = meta.permissions().mode() | 0o200;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
}
