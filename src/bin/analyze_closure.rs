//! Analyze a Nix closure and compute a dedup plan against a Flatpak runtime.
//!
//! Walks every store path in the closure, classifies each file as
//! keep / drop / rewrite by comparing against the runtime index, then
//! prunes orphaned transitive dependencies and emits a JSON plan consumed
//! by `nix2flatpak-rewrite-for-flatpak`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process;

use anyhow::{Context, Result};
use clap::Parser;
use regex::Regex;
use serde_json::{json, Map, Value};
use walkdir::WalkDir;

use nix2flatpak::{extract_soname, store_path_hash};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(about = "Analyze Nix closure for Flatpak deduplication")]
struct Cli {
    #[arg(long)]
    package: String,
    #[arg(long)]
    runtime_index: PathBuf,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    closure_file: Option<PathBuf>,
    /// Downgrade ABI compatibility errors to warnings
    #[arg(long)]
    warn_abi_only: bool,
}

// ---------------------------------------------------------------------------
// Version helpers
// ---------------------------------------------------------------------------

fn parse_version(s: &str) -> Vec<u32> {
    s.split('.').filter_map(|p| p.parse().ok()).collect()
}

/// Find a package in the closure by name and extract its version.
fn find_nix_package_version(closure: &[String], pkg_name: &str) -> Option<String> {
    let re = Regex::new(r"^(\d+\.\d+(?:\.\d+)*)").unwrap();
    for sp in closure {
        let basename = sp.rsplit('/').next()?;
        // Hash is always 32 chars + '-'
        if basename.len() <= 33 || basename.as_bytes()[32] != b'-' {
            continue;
        }
        let rest = &basename[33..]; // name-version part
        if !rest.starts_with(&format!("{pkg_name}-")) {
            continue;
        }
        let version_part = &rest[pkg_name.len() + 1..];
        if let Some(m) = re.find(version_part) {
            return Some(m.as_str().to_string());
        }
    }
    None
}

/// Find the libstdc++ file version (e.g. "6.0.34") from a gcc-libs store path.
fn find_libstdcxx_file_version(closure: &[String]) -> Option<String> {
    let re = Regex::new(r"^libstdc\+\+\.so\.(\d+\.\d+\.\d+)$").unwrap();
    for sp in closure {
        let basename = sp.rsplit('/').next().unwrap_or("");
        if basename.len() <= 33 || basename.as_bytes()[32] != b'-' {
            continue;
        }
        let rest = &basename[33..];
        if !rest.starts_with("gcc-libs-") && !rest.starts_with("gcc-unwrapped-") {
            continue;
        }
        let lib_dir = Path::new(sp).join("lib");
        let Ok(entries) = fs::read_dir(&lib_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(caps) = re.captures(&name) {
                return Some(caps[1].to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// ABI compatibility checks
// ---------------------------------------------------------------------------

fn check_glibc_compat(closure: &[String], runtime_index: &Value, fatal: bool) {
    let versions = &runtime_index["versions"];
    let Some(runtime_glibc) = versions["glibc"].as_str() else {
        eprintln!("WARNING: Runtime index has no glibc version info, skipping glibc ABI check");
        return;
    };
    let Some(nix_glibc) = find_nix_package_version(closure, "glibc") else {
        eprintln!("WARNING: Could not find glibc in Nix closure, skipping glibc ABI check");
        return;
    };
    eprintln!("glibc: Nix={nix_glibc}, Runtime={runtime_glibc}");
    if parse_version(&nix_glibc) > parse_version(runtime_glibc) {
        let level = if fatal { "ERROR" } else { "WARNING" };
        eprintln!(
            "{level}: Nix glibc ({nix_glibc}) is newer than the Flatpak runtime's \
             glibc ({runtime_glibc}). Binaries may reference GLIBC_{nix_glibc} symbols \
             not present in the runtime. Pin nixpkgs to a revision with glibc <= \
             {runtime_glibc} or use a newer Flatpak runtime."
        );
        if fatal {
            process::exit(1);
        }
    }
}

fn check_libstdcxx_compat(closure: &[String], runtime_index: &Value, fatal: bool) {
    let versions = &runtime_index["versions"];
    let Some(runtime_stdcxx) = versions["libstdcxx"].as_str() else {
        eprintln!(
            "WARNING: Runtime index has no libstdc++ version info, skipping libstdc++ ABI check"
        );
        return;
    };
    let Some(nix_stdcxx) = find_libstdcxx_file_version(closure) else {
        eprintln!(
            "WARNING: Could not find libstdc++ in Nix closure, skipping libstdc++ ABI check"
        );
        return;
    };
    eprintln!("libstdc++: Nix={nix_stdcxx}, Runtime={runtime_stdcxx}");
    if parse_version(&nix_stdcxx) > parse_version(runtime_stdcxx) {
        let level = if fatal { "ERROR" } else { "WARNING" };
        eprintln!(
            "{level}: Nix libstdc++ ({nix_stdcxx}) is newer than the Flatpak runtime's \
             ({runtime_stdcxx}). C++ binaries may reference GLIBCXX/CXXABI symbols not \
             present in the runtime. Pin nixpkgs to a revision with a compatible GCC or \
             use a newer Flatpak runtime."
        );
        if fatal {
            process::exit(1);
        }
    }
}

fn check_qt_compat(closure: &[String], runtime_index: &Value) {
    let Some(runtime_qt) = runtime_index["versions"]["qt"].as_str() else {
        return;
    };
    let Some(nix_qt) = find_nix_package_version(closure, "qtbase") else {
        return;
    };
    eprintln!("Qt: Nix={nix_qt}, Runtime={runtime_qt}");

    let nix_minor: Vec<&str> = nix_qt.splitn(3, '.').take(2).collect();
    let rt_minor: Vec<&str> = runtime_qt.splitn(3, '.').take(2).collect();
    if nix_minor != rt_minor {
        eprintln!(
            "WARNING: Nix Qt ({nix_qt}) and Flatpak runtime Qt ({runtime_qt}) differ in \
             major.minor version. Apps using Qt private APIs (common in KDE) may crash \
             with undefined symbol errors. Consider matching Qt versions between nixpkgs \
             and the Flatpak runtime."
        );
    }
}

// ---------------------------------------------------------------------------
// Build-artifact detection
// ---------------------------------------------------------------------------

fn is_build_or_dev_artifact(rel_path: &str) -> bool {
    const DIR_PATTERNS: &[&str] = &[
        "/include/",
        "/lib/pkgconfig/",
        "/share/pkgconfig/",
        "/share/man/",
        "/share/doc/",
        "/share/info/",
        "/share/aclocal/",
        "/share/devhelp/",
        "/lib/cmake/",
        "/share/cmake/",
        "/share/vala/",
        "/share/gir-1.0/",
        "/share/gtk-doc/",
        "/share/bash-completion/",
        "/share/zsh/",
        "/share/fish/",
        "/nix-support/",
    ];
    const SUFFIXES: &[&str] = &[
        ".h", ".hpp", ".hxx", ".pc", ".la", ".a", ".o", ".cmake", ".gir",
    ];

    let padded = format!("/{rel_path}/");
    for pat in DIR_PATTERNS {
        if padded.contains(pat) {
            return true;
        }
    }
    for sfx in SUFFIXES {
        if rel_path.ends_with(sfx) {
            return true;
        }
    }
    false
}

/// Plugin .so files are loaded via dlopen, not DT_NEEDED.
/// If the parent library is provided by the runtime, we should drop
/// the plugin in favour of the runtime's own copy.
fn is_plugin_so(rel_path: &str) -> bool {
    let parts: Vec<&str> = rel_path.split('/').collect();
    if parts.first() != Some(&"lib") {
        return false;
    }
    // .so files in subdirectories of lib/ are always plugins
    if parts.len() >= 3 {
        return true;
    }
    // NSS modules are glibc plugins (loaded via nsswitch.conf)
    let fname = parts.last().unwrap_or(&"");
    if fname.starts_with("libnss_") && fname.contains(".so.") {
        return true;
    }
    false
}

fn is_executable_path(rel_path: &str) -> bool {
    rel_path.starts_with("bin/")
        || rel_path.starts_with("sbin/")
        || rel_path.starts_with("libexec/")
        || rel_path.contains("/bin/")
        || rel_path.contains("/sbin/")
        || rel_path.contains("/libexec/")
}

// ---------------------------------------------------------------------------
// Store-path classification
// ---------------------------------------------------------------------------

struct ClassifyResult {
    store_path: String,
    classification: &'static str,
    reason: &'static str,
    kept_files: Vec<String>,
    dropped_files: Vec<String>,
    matched_sonames: Vec<String>,
    rewrites: Vec<Value>,
}

fn classify_store_path(
    store_path: &str,
    runtime_index: &Value,
    target_package: &str,
    essential_deps: &HashSet<String>,
    versioned_soname_re: &Regex,
) -> ClassifyResult {
    let path = Path::new(store_path);
    let mut result = ClassifyResult {
        store_path: store_path.to_string(),
        classification: "drop",
        reason: "not-found",
        kept_files: Vec::new(),
        dropped_files: Vec::new(),
        matched_sonames: Vec::new(),
        rewrites: Vec::new(),
    };

    if !path.exists() {
        return result;
    }

    let runtime_sonames = &runtime_index["sonames"];
    let runtime_executables = &runtime_index["executables"];
    let is_target = store_path == target_package;

    // Collect all files and their metadata in one pass
    struct FileInfo {
        filepath: PathBuf,
        fname: String,
        rel_path: String,
    }

    let mut all_files: Vec<FileInfo> = Vec::new();
    let mut dropped_sonames_in_path: HashSet<String> = HashSet::new();
    let mut has_matched_versioned_soname = false;

    for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
        let ft = entry.file_type();
        // Include regular files and symlinks that resolve to files.
        // WalkDir (follow_links=false) reports symlinks as symlinks, but
        // Python's os.walk + is_file() follows symlinks — we must match
        // that behavior so SONAME symlinks (libfoo.so.3 → libfoo.so.3.1.0)
        // appear in keepFiles and get copied to the output.
        if !ft.is_file() && !(ft.is_symlink() && entry.path().is_file()) {
            continue;
        }
        let filepath = entry.path().to_path_buf();
        let fname = entry.file_name().to_string_lossy().to_string();
        let rel_path = filepath
            .strip_prefix(path)
            .unwrap_or(&filepath)
            .to_string_lossy()
            .to_string();

        // First pass: identify SONAMEs that match the runtime
        if fname.contains(".so") && !fname.ends_with(".py") {
            if let Some(soname) = extract_soname(&filepath) {
                if runtime_sonames.get(&soname).is_some() {
                    dropped_sonames_in_path.insert(soname.clone());
                    if versioned_soname_re.is_match(&soname) {
                        has_matched_versioned_soname = true;
                    }
                }
            }
        }

        all_files.push(FileInfo {
            filepath,
            fname,
            rel_path,
        });
    }

    let mut has_any_content = false;

    for fi in &all_files {
        has_any_content = true;

        // Build/dev artifacts: always drop
        if is_build_or_dev_artifact(&fi.rel_path) {
            result.dropped_files.push(fi.rel_path.clone());
            continue;
        }

        // --- Libraries ---
        if fi.fname.contains(".so") && !fi.fname.ends_with(".py") {
            if let Some(soname) = extract_soname(&fi.filepath) {
                if runtime_sonames.get(&soname).is_some() {
                    result.matched_sonames.push(soname);
                    result.dropped_files.push(fi.rel_path.clone());
                    continue;
                }
                // Has a soname not in runtime, but if it's a plugin whose parent
                // library IS runtime-provided, drop it.
                if has_matched_versioned_soname && is_plugin_so(&fi.rel_path) {
                    result.dropped_files.push(fi.rel_path.clone());
                    continue;
                }
                result.kept_files.push(fi.rel_path.clone());
                continue;
            }

            // .so without SONAME (dev symlink or similar)
            if fi.filepath.symlink_metadata().map_or(false, |m| m.file_type().is_symlink()) {
                // Check if symlink target's soname is being dropped
                if let Ok(target) = fi.filepath.canonicalize() {
                    if let Some(target_soname) = extract_soname(&target) {
                        if runtime_sonames.get(&target_soname).is_some() {
                            result.dropped_files.push(fi.rel_path.clone());
                            continue;
                        }
                    }
                }
                result.dropped_files.push(fi.rel_path.clone());
            } else if !dropped_sonames_in_path.is_empty() {
                // Unversioned .so alongside dropped versioned .so → dev link
                result.dropped_files.push(fi.rel_path.clone());
            } else {
                result.kept_files.push(fi.rel_path.clone());
            }
            continue;
        }

        // --- Executables ---
        if is_executable_path(&fi.rel_path) {
            let exec_name = fi.filepath.file_name().unwrap().to_string_lossy();
            if runtime_executables.get(exec_name.as_ref()).is_some() && !is_target {
                let rt_path = runtime_executables[exec_name.as_ref()]["path"]
                    .as_str()
                    .unwrap_or("");
                result.rewrites.push(json!({
                    "from": fi.filepath.to_string_lossy(),
                    "to": format!("/usr/{rt_path}"),
                    "type": "executable",
                }));
                result.dropped_files.push(fi.rel_path.clone());
                continue;
            } else if !is_target {
                let is_direct_dep = essential_deps.contains(store_path);
                if is_direct_dep {
                    result.kept_files.push(fi.rel_path.clone());
                } else {
                    result.dropped_files.push(fi.rel_path.clone());
                }
                continue;
            } else {
                result.kept_files.push(fi.rel_path.clone());
                continue;
            }
        }

        // --- Data files ---
        let runtime_data = &runtime_index["data"];

        // GLib schemas
        if fi.rel_path.contains("share/glib-2.0/schemas/") && fi.fname.ends_with(".xml") {
            if let Some(schemas) = runtime_data["glib-schemas"].as_array() {
                if schemas.iter().any(|s| s.as_str() == Some(&fi.fname)) {
                    result.dropped_files.push(fi.rel_path.clone());
                    continue;
                }
            }
        }

        // Typelibs
        if fi.rel_path.contains("girepository-1.0/") && fi.fname.ends_with(".typelib") {
            if let Some(typelibs) = runtime_data["typelibs"].as_array() {
                if typelibs.iter().any(|s| s.as_str() == Some(&fi.fname)) {
                    result.dropped_files.push(fi.rel_path.clone());
                    continue;
                }
            }
        }

        // Locale data
        if fi.rel_path.contains("share/locale/") {
            result.dropped_files.push(fi.rel_path.clone());
            continue;
        }

        // Icon themes
        if fi.rel_path.contains("share/icons/") {
            if let Some(rt_themes) = runtime_data["icon-themes"].as_array() {
                let parts: Vec<&str> = fi.rel_path.split('/').collect();
                if let Some(icons_idx) = parts.iter().position(|&p| p == "icons") {
                    if icons_idx + 1 < parts.len() {
                        let theme = parts[icons_idx + 1];
                        if !is_target
                            && rt_themes.iter().any(|t| t.as_str() == Some(theme))
                        {
                            result.dropped_files.push(fi.rel_path.clone());
                            continue;
                        }
                    }
                }
            }
        }

        // Default: keep
        result.kept_files.push(fi.rel_path.clone());
    }

    // Second pass: if primary content is runtime-provided and no unique
    // libraries or executables remain, drop everything.
    if !is_target && !result.kept_files.is_empty() {
        let has_unique_libs = result
            .kept_files
            .iter()
            .any(|f| f.contains(".so") && !f.ends_with(".py"));
        let has_unique_executables = result.kept_files.iter().any(|f| is_executable_path(f));
        let has_primary_content_dropped = has_matched_versioned_soname
            || result
                .dropped_files
                .iter()
                .any(|f| is_executable_path(f));

        // Drop everything when:
        // - primary content (libs or executables) was dropped, AND
        // - no unique .so files remain, AND
        // - no unique executables remain, OR the path is a pure library path
        //   (all versioned sonames matched the runtime) — in that case,
        //   executables are incidental utilities (like glibc's ldd or
        //   systemd's systemctl), not app binaries.
        //   Paths like electron-unwrapped that have unique .so files (e.g.
        //   libffmpeg.so) are NOT affected because has_unique_libs is true.
        if has_primary_content_dropped
            && !has_unique_libs
            && (!has_unique_executables || has_matched_versioned_soname)
        {
            result.dropped_files.extend(result.kept_files.drain(..));
        }
    }

    if !has_any_content {
        result.classification = "drop";
        result.reason = "empty";
        return result;
    }

    if is_target {
        result.classification = "keep";
        result.reason = "target-package";
        // Target keeps everything
        result.kept_files.extend(result.dropped_files.drain(..));
        return result;
    }

    match (result.kept_files.is_empty(), result.dropped_files.is_empty()) {
        (true, true) => {
            result.classification = "drop";
            result.reason = "no-content";
        }
        (true, false) => {
            result.classification = "drop";
            result.reason = "all-content-in-runtime";
        }
        (false, true) => {
            result.classification = "keep";
            result.reason = "no-runtime-match";
        }
        (false, false) => {
            result.classification = "partial";
            result.reason = "";
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Closure computation
// ---------------------------------------------------------------------------

/// Parse the exportReferencesGraph format or fall back to `nix-store -qR`.
fn compute_closure(
    package: &str,
    closure_file: Option<&Path>,
) -> Result<(Vec<String>, HashMap<String, Vec<String>>)> {
    let mut refs: HashMap<String, Vec<String>> = HashMap::new();

    if let Some(cf) = closure_file {
        let content = fs::read_to_string(cf).context("reading closure file")?;
        let lines: Vec<&str> = content.lines().collect();
        let mut paths: BTreeSet<String> = BTreeSet::new();

        if lines.first().map_or(false, |l| l.starts_with("/nix/store/")) {
            let mut i = 0;
            while i < lines.len() {
                let line = lines[i].trim();
                if line.starts_with("/nix/store/") {
                    let sp = line.to_string();
                    paths.insert(sp.clone());
                    i += 1; // skip deriver line
                    if i < lines.len() {
                        i += 1;
                    }
                    // Parse num_refs
                    if i < lines.len() {
                        if let Ok(num_refs) = lines[i].trim().parse::<usize>() {
                            i += 1;
                            let mut sp_refs = Vec::new();
                            for _ in 0..num_refs {
                                if i < lines.len() {
                                    let r = lines[i].trim();
                                    if r.starts_with("/nix/store/") {
                                        sp_refs.push(r.to_string());
                                    }
                                    i += 1;
                                }
                            }
                            refs.insert(sp, sp_refs);
                        } else {
                            i += 1;
                        }
                    }
                } else {
                    i += 1;
                }
            }
        }
        Ok((paths.into_iter().collect(), refs))
    } else {
        let output = process::Command::new("nix-store")
            .args(["-qR", package])
            .output()
            .context("running nix-store -qR")?;
        anyhow::ensure!(output.status.success(), "nix-store -qR failed");
        let mut paths: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|s| s.to_string())
            .collect();
        paths.sort();

        for sp in &paths {
            let output = process::Command::new("nix-store")
                .args(["-q", "--references", sp])
                .output();
            match output {
                Ok(o) if o.status.success() => {
                    refs.insert(
                        sp.clone(),
                        String::from_utf8_lossy(&o.stdout)
                            .lines()
                            .filter(|l| !l.is_empty() && *l != sp)
                            .map(|s| s.to_string())
                            .collect(),
                    );
                }
                _ => {
                    refs.insert(sp.clone(), Vec::new());
                }
            }
        }
        Ok((paths, refs))
    }
}

// ---------------------------------------------------------------------------
// Orphan detection
// ---------------------------------------------------------------------------

/// Build reverse reference map (who depends on each path), excluding self-refs.
fn compute_reverse_refs(
    refs: &HashMap<String, Vec<String>>,
) -> HashMap<String, HashSet<String>> {
    let mut reverse: HashMap<String, HashSet<String>> = HashMap::new();
    for (sp, deps) in refs {
        for dep in deps {
            if dep != sp {
                reverse
                    .entry(dep.clone())
                    .or_default()
                    .insert(sp.clone());
            }
        }
    }
    reverse
}

/// Iteratively drop kept/partial paths only referenced by already-dropped paths.
fn drop_orphaned_paths(
    keep: &mut Vec<Value>,
    drop_list: &mut Vec<Value>,
    partial: &mut Vec<Value>,
    refs: &HashMap<String, Vec<String>>,
    target_package: &str,
) {
    let reverse_refs = compute_reverse_refs(refs);
    let mut dropped_set: HashSet<String> = drop_list
        .iter()
        .filter_map(|e| e["storePath"].as_str().map(String::from))
        .collect();
    let mut kept_set: HashSet<String> = keep
        .iter()
        .filter_map(|e| e["storePath"].as_str().map(String::from))
        .collect();
    let mut partial_set: HashSet<String> = partial
        .iter()
        .filter_map(|e| e["storePath"].as_str().map(String::from))
        .collect();

    let mut total_orphaned = 0u32;
    loop {
        let mut newly_orphaned = Vec::new();
        for sp in kept_set.iter().chain(partial_set.iter()) {
            if sp == target_package {
                continue;
            }
            let referrers = reverse_refs.get(sp);
            if let Some(referrers) = referrers {
                if !referrers.is_empty()
                    && referrers.iter().all(|r| dropped_set.contains(r))
                {
                    newly_orphaned.push(sp.clone());
                }
            }
        }
        if newly_orphaned.is_empty() {
            break;
        }
        for sp in &newly_orphaned {
            total_orphaned += 1;
            dropped_set.insert(sp.clone());
            kept_set.remove(sp);
            partial_set.remove(sp);
        }
    }

    if total_orphaned > 0 {
        eprintln!("Dropped {total_orphaned} orphaned transitive dependencies");
    }

    // Rebuild lists
    let original_drop_paths: HashSet<String> = drop_list
        .iter()
        .filter_map(|e| e["storePath"].as_str().map(String::from))
        .collect();

    keep.retain(|e| {
        e["storePath"]
            .as_str()
            .map_or(true, |sp| kept_set.contains(sp))
    });
    partial.retain(|e| {
        e["storePath"]
            .as_str()
            .map_or(true, |sp| partial_set.contains(sp))
    });
    // Add newly orphaned paths to drop list
    let mut new_orphans: Vec<String> = dropped_set
        .difference(&original_drop_paths)
        .cloned()
        .collect();
    new_orphans.sort();
    for sp in new_orphans {
        drop_list.push(json!({"storePath": sp, "reason": "orphaned-transitive"}));
    }
}

// ---------------------------------------------------------------------------
// Reference scanning
// ---------------------------------------------------------------------------

/// Scan kept files for references to dropped store path hashes.
///
/// Instead of searching for each hash individually (O(files × hashes × size)),
/// we scan each file once for the `/nix/store/` marker and check the 32-char
/// hash that follows against a HashSet — O(files × size).
fn scan_for_references(kept: &[Value], dropped: &[Value]) -> Vec<Value> {
    let mut drop_hash_map: HashMap<String, String> = HashMap::new();
    let mut drop_hash_set: HashSet<Vec<u8>> = HashSet::new();
    for entry in dropped {
        if let Some(sp) = entry["storePath"].as_str() {
            let h = store_path_hash(sp).to_string();
            drop_hash_set.insert(h.as_bytes().to_vec());
            drop_hash_map.insert(h, sp.to_string());
        }
    }

    let marker = b"/nix/store/";
    let mut additional_rewrites = Vec::new();
    let mut found_hashes: HashSet<String> = HashSet::new();

    for entry in kept {
        let Some(sp) = entry["storePath"].as_str() else {
            continue;
        };
        let path = Path::new(sp);
        if !path.exists() {
            continue;
        }

        for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.path().symlink_metadata().map_or(true, |m| m.file_type().is_symlink()) {
                continue;
            }

            let Ok(content) = fs::read(entry.path()) else {
                continue;
            };

            // Scan for /nix/store/ markers and check the following 32-byte hash
            let mut pos = 0;
            while pos + marker.len() + 32 <= content.len() {
                if let Some(idx) = content[pos..].windows(marker.len()).position(|w| w == marker) {
                    let hash_start = pos + idx + marker.len();
                    if hash_start + 32 <= content.len() {
                        let hash_bytes = &content[hash_start..hash_start + 32];
                        if drop_hash_set.contains(hash_bytes) {
                            let hash_str = String::from_utf8_lossy(hash_bytes).to_string();
                            if !found_hashes.contains(&hash_str) {
                                if let Some(drop_path) = drop_hash_map.get(&hash_str) {
                                    found_hashes.insert(hash_str);
                                    additional_rewrites.push(json!({
                                        "from": drop_path,
                                        "to": format!("/app{drop_path}"),
                                        "type": "store-path-reference",
                                        "referencedBy": entry.path().to_string_lossy(),
                                    }));
                                }
                            }
                        }
                    }
                    pos = pos + idx + 1;
                } else {
                    break;
                }
            }
        }
    }
    additional_rewrites
}

// ---------------------------------------------------------------------------
// Size calculation
// ---------------------------------------------------------------------------

fn get_path_size(store_path: &str) -> u64 {
    let path = Path::new(store_path);
    if !path.exists() {
        return 0;
    }
    WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.path().symlink_metadata().ok())
        .map(|m| m.len())
        .sum()
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Resolve symlinks for the package path
    let package = fs::canonicalize(&cli.package)
        .with_context(|| format!("resolving package path {}", cli.package))?
        .to_string_lossy()
        .to_string();

    let runtime_index: Value =
        serde_json::from_str(&fs::read_to_string(&cli.runtime_index)?)?;

    let (closure, refs) = compute_closure(&package, cli.closure_file.as_deref())?;
    eprintln!("Closure has {} store paths", closure.len());

    // ABI compatibility checks
    let abi_fatal = !cli.warn_abi_only;
    check_glibc_compat(&closure, &runtime_index, abi_fatal);
    check_libstdcxx_compat(&closure, &runtime_index, abi_fatal);
    check_qt_compat(&closure, &runtime_index);

    // Essential deps: 2 levels of direct dependencies.
    // Keeps executables in wrapper chains (e.g. signal → electron → electron-unwrapped)
    // while still dropping executables from deep transitive deps.
    let mut essential_deps: HashSet<String> = HashSet::new();
    if !refs.is_empty() {
        if let Some(direct) = refs.get(&package) {
            for dep in direct {
                essential_deps.insert(dep.clone());
                if let Some(dep2s) = refs.get(dep) {
                    for dep2 in dep2s {
                        essential_deps.insert(dep2.clone());
                    }
                }
            }
        }
        eprintln!(
            "Target has {} essential dependencies (2-level)",
            essential_deps.len()
        );
    }

    let mut keep: Vec<Value> = Vec::new();
    let mut drop_list: Vec<Value> = Vec::new();
    let mut partial: Vec<Value> = Vec::new();
    let mut all_rewrites: Vec<Value> = Vec::new();
    let versioned_soname_re = Regex::new(r"\.so\.\d+").unwrap();

    for sp in &closure {
        let cr = classify_store_path(sp, &runtime_index, &package, &essential_deps, &versioned_soname_re);

        match cr.classification {
            "keep" => {
                keep.push(json!({"storePath": cr.store_path, "reason": cr.reason}));
            }
            "drop" => {
                let mut entry = json!({"storePath": cr.store_path, "reason": cr.reason});
                if !cr.matched_sonames.is_empty() {
                    let mut sorted = cr.matched_sonames;
                    sorted.sort();
                    entry["matchedSonames"] = json!(sorted);
                }
                drop_list.push(entry);
                all_rewrites.extend(cr.rewrites);
            }
            "partial" => {
                let mut kf = cr.kept_files;
                let mut df = cr.dropped_files;
                kf.sort();
                df.sort();
                partial.push(json!({
                    "storePath": cr.store_path,
                    "keepFiles": kf,
                    "dropFiles": df,
                }));
                all_rewrites.extend(cr.rewrites);
            }
            _ => {
                drop_list.push(json!({"storePath": cr.store_path, "reason": cr.reason}));
            }
        }
    }

    // Drop orphaned transitive dependencies
    if !refs.is_empty() {
        eprintln!("Checking for orphaned transitive dependencies...");
        drop_orphaned_paths(&mut keep, &mut drop_list, &mut partial, &refs, &package);
    }

    // Reference scan
    eprintln!("Scanning for cross-references...");
    let kept_and_partial: Vec<Value> = keep.iter().chain(partial.iter()).cloned().collect();
    let ref_rewrites = scan_for_references(&kept_and_partial, &drop_list);
    all_rewrites.extend(ref_rewrites);

    // Deduplicate rewrites by 'from' field
    let mut seen_from: HashSet<String> = HashSet::new();
    let mut unique_rewrites: Vec<Value> = Vec::new();
    for r in all_rewrites {
        if let Some(from) = r["from"].as_str() {
            if seen_from.insert(from.to_string()) {
                unique_rewrites.push(r);
            }
        }
    }

    // Compute stats
    let closure_size: u64 = closure.iter().map(|sp| get_path_size(sp)).sum();
    let mut kept_size: u64 = keep.iter().map(|e| {
        e["storePath"].as_str().map_or(0, get_path_size)
    }).sum();
    for p in &partial {
        if let (Some(sp), Some(keep_files)) = (p["storePath"].as_str(), p["keepFiles"].as_array()) {
            let base = Path::new(sp);
            for kf in keep_files {
                if let Some(kf_str) = kf.as_str() {
                    let fpath = base.join(kf_str);
                    if let Ok(meta) = fpath.symlink_metadata() {
                        kept_size += meta.len();
                    }
                }
            }
        }
    }
    let dedup_ratio = if closure_size > 0 {
        (closure_size - kept_size) as f64 / closure_size as f64 * 100.0
    } else {
        0.0
    };

    keep.sort_by(|a, b| a["storePath"].as_str().cmp(&b["storePath"].as_str()));
    drop_list.sort_by(|a, b| a["storePath"].as_str().cmp(&b["storePath"].as_str()));
    partial.sort_by(|a, b| a["storePath"].as_str().cmp(&b["storePath"].as_str()));
    unique_rewrites.sort_by(|a, b| a["from"].as_str().cmp(&b["from"].as_str()));

    let mut plan = Map::new();

    // Add runtime info from metadata if available
    let runtime_name = runtime_index["metadata"]["Runtime"]["name"]
        .as_str()
        .unwrap_or("detected-from-index");
    plan.insert("runtime".into(), json!(runtime_name));
    plan.insert("package".into(), json!(package));
    plan.insert("keep".into(), json!(keep));
    plan.insert("drop".into(), json!(drop_list));
    plan.insert("partial".into(), json!(partial));
    plan.insert("rewrites".into(), json!(unique_rewrites));
    plan.insert(
        "stats".into(),
        json!({
            "closurePaths": closure.len(),
            "closureSizeBytes": closure_size,
            "keptPaths": keep.len() + partial.len(),
            "keptSizeBytes": kept_size,
            "dedupRatio": format!("{dedup_ratio:.1}%"),
        }),
    );

    let plan_json = serde_json::to_string_pretty(&Value::Object(plan))?;
    fs::write(&cli.output, format!("{plan_json}\n"))?;

    eprintln!(
        "Keep: {} paths, Drop: {} paths, Partial: {} paths",
        keep.len(),
        drop_list.len(),
        partial.len()
    );
    eprintln!("Rewrites: {}", unique_rewrites.len());
    eprintln!("Dedup ratio: {dedup_ratio:.1}%");

    Ok(())
}
