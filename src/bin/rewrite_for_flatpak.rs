//! Rewrite Nix store paths for Flatpak packaging.
//!
//! Copies kept store paths from the dedup plan, then rewrites every ELF
//! binary (interpreter + RPATH via patchelf), replaces Nix compiled wrappers
//! with shell scripts, patches text-file references, and creates the
//! symlink structure Flatpak expects under `/app/`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::Parser;
use goblin::elf::Elf;
use regex::Regex;
use serde_json::Value;
use walkdir::WalkDir;

use nix2flatpak::{copy_tree, is_elf, is_text_file, make_writable, store_path_hash};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(about = "Rewrite Nix store paths for Flatpak")]
struct Cli {
    #[arg(long)]
    dedup_plan: PathBuf,
    #[arg(long)]
    output_dir: PathBuf,
    #[arg(long)]
    arch_triplet: String,
    #[arg(long, default_value = "patchelf")]
    patchelf: String,
    #[arg(long)]
    runtime_index: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Rewrite map construction
// ---------------------------------------------------------------------------

fn build_rewrite_map(plan: &Value) -> HashMap<String, String> {
    let mut map = HashMap::new();

    // Explicit rewrites from the plan
    if let Some(rewrites) = plan["rewrites"].as_array() {
        for r in rewrites {
            if let (Some(from), Some(to)) = (r["from"].as_str(), r["to"].as_str()) {
                map.insert(from.to_string(), to.to_string());
            }
        }
    }

    // Keep paths: /nix/store/… → /app/nix/store/…
    if let Some(keep) = plan["keep"].as_array() {
        for k in keep {
            if let Some(sp) = k["storePath"].as_str() {
                map.insert(sp.to_string(), format!("/app{sp}"));
            }
        }
    }

    // Partial paths: same treatment
    if let Some(partial) = plan["partial"].as_array() {
        for p in partial {
            if let Some(sp) = p["storePath"].as_str() {
                map.insert(sp.to_string(), format!("/app{sp}"));
            }
        }
    }

    map
}

fn build_drop_hashes(plan: &Value) -> HashSet<String> {
    let mut hashes = HashSet::new();
    if let Some(drop) = plan["drop"].as_array() {
        for d in drop {
            if let Some(sp) = d["storePath"].as_str() {
                hashes.insert(store_path_hash(sp).to_string());
            }
        }
    }
    hashes
}

// ---------------------------------------------------------------------------
// Copy store paths
// ---------------------------------------------------------------------------

fn copy_store_paths(plan: &Value, output_dir: &Path) -> Result<()> {
    let nix_store_dir = output_dir.join("nix/store");
    fs::create_dir_all(&nix_store_dir)?;

    // Full keep paths
    if let Some(keep) = plan["keep"].as_array() {
        for k in keep {
            let Some(sp_str) = k["storePath"].as_str() else {
                continue;
            };
            let sp = Path::new(sp_str);
            if !sp.exists() {
                eprintln!("WARNING: store path not found: {sp_str}");
                continue;
            }
            let dest = nix_store_dir.join(sp.file_name().unwrap());
            if !dest.exists() {
                if sp.is_dir() {
                    copy_tree(sp, &dest)
                        .with_context(|| format!("copying store path {sp_str}"))?;
                } else {
                    // Store path is a single file (e.g. a script)
                    fs::copy(sp, &dest)
                        .with_context(|| format!("copying store file {sp_str}"))?;
                }
            }
            // Make all files writable for patching
            for entry in WalkDir::new(&dest).into_iter().filter_map(|e| e.ok()) {
                if entry.file_type().is_file() {
                    make_writable(entry.path());
                }
            }
        }
    }

    // Partial paths: copy only kept files
    if let Some(partial) = plan["partial"].as_array() {
        for p in partial {
            let Some(sp_str) = p["storePath"].as_str() else {
                continue;
            };
            let sp = Path::new(sp_str);
            if !sp.exists() {
                continue;
            }
            let dest = nix_store_dir.join(sp.file_name().unwrap());
            fs::create_dir_all(&dest)?;

            // Preserve top-level directory symlinks (e.g. lib64 → lib)
            if let Ok(entries) = fs::read_dir(sp) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if let Ok(meta) = path.symlink_metadata() {
                        if meta.file_type().is_symlink() && path.is_dir() {
                            let link_target = fs::read_link(&path)?;
                            let dst_link = dest.join(entry.file_name());
                            if !dst_link.exists() && !dst_link.symlink_metadata().is_ok() {
                                std::os::unix::fs::symlink(&link_target, &dst_link)?;
                            }
                        }
                    }
                }
            }

            if let Some(keep_files) = p["keepFiles"].as_array() {
                for kf in keep_files {
                    let Some(rel_file) = kf.as_str() else {
                        continue;
                    };
                    let src_file = sp.join(rel_file);
                    let dst_file = dest.join(rel_file);
                    if !src_file.exists() {
                        continue;
                    }
                    if let Some(parent) = dst_file.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    if let Ok(meta) = src_file.symlink_metadata() {
                        if meta.file_type().is_symlink() {
                            let link_target = fs::read_link(&src_file)?;
                            if dst_file.exists() || dst_file.symlink_metadata().is_ok() {
                                fs::remove_file(&dst_file)?;
                            }
                            std::os::unix::fs::symlink(&link_target, &dst_file)?;
                        } else {
                            fs::copy(&src_file, &dst_file)?;
                            make_writable(&dst_file);
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// ELF rewriting
// ---------------------------------------------------------------------------

fn rewrite_elf(
    filepath: &Path,
    _rewrite_map: &HashMap<String, String>,
    drop_hashes: &HashSet<String>,
    arch_triplet: &str,
    patchelf: &str,
) {
    if !is_elf(filepath) {
        return;
    }

    // Rewrite interpreter
    if let Ok(output) = Command::new(patchelf)
        .args(["--print-interpreter", &filepath.to_string_lossy()])
        .output()
    {
        if output.status.success() {
            let interp = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if interp.contains("/nix/store/") {
                let ld_name = if arch_triplet.contains("aarch64") {
                    "ld-linux-aarch64.so.1"
                } else {
                    "ld-linux-x86-64.so.2"
                };
                let new_interp = format!("/usr/lib/{arch_triplet}/{ld_name}");
                let _ = Command::new(patchelf)
                    .args(["--set-interpreter", &new_interp, &filepath.to_string_lossy()])
                    .output();
            }
        }
    }

    // Rewrite RPATH
    let Ok(output) = Command::new(patchelf)
        .args(["--print-rpath", &filepath.to_string_lossy()])
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }

    let old_rpath = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if old_rpath.is_empty() {
        return;
    }

    let mut new_entries = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for entry in old_rpath.split(':') {
        if entry.is_empty() {
            continue;
        }

        let new_entry = if entry.starts_with("/nix/store/") {
            // Extract hash from the store path component
            let after_store = &entry["/nix/store/".len()..];
            let basename = after_store.split('/').next().unwrap_or("");
            let hash_part = match basename.find('-') {
                Some(idx) => &basename[..idx],
                None => basename,
            };

            if drop_hashes.contains(hash_part) {
                format!("/usr/lib/{arch_triplet}")
            } else {
                format!("/app{entry}")
            }
        } else {
            entry.to_string()
        };

        if seen.insert(new_entry.clone()) {
            new_entries.push(new_entry);
        }
    }

    // Always include runtime and app lib paths
    let runtime_lib = format!("/usr/lib/{arch_triplet}");
    if seen.insert(runtime_lib.clone()) {
        new_entries.push(runtime_lib);
    }
    let app_lib = "/app/lib".to_string();
    if seen.insert(app_lib.clone()) {
        new_entries.push(app_lib);
    }

    let new_rpath = new_entries.join(":");
    if new_rpath != old_rpath {
        if let Err(e) = Command::new(patchelf)
            .args(["--set-rpath", &new_rpath, &filepath.to_string_lossy()])
            .output()
            .and_then(|o| {
                if o.status.success() {
                    Ok(o)
                } else {
                    Err(std::io::Error::other(format!(
                        "patchelf exited with {}",
                        o.status
                    )))
                }
            })
        {
            eprintln!("WARNING: patchelf failed on {}: {e}", filepath.display());
        }
    }
}

// ---------------------------------------------------------------------------
// Nix compiled wrapper handling
// ---------------------------------------------------------------------------

/// Detect Nix's `makeCWrapper` ELF stubs by scanning for marker strings.
/// Detect Nix's `makeCWrapper` ELF stubs by scanning for marker strings.
/// Returns the file data if it is a wrapper (avoids re-reading the file later).
fn check_nix_compiled_wrapper(filepath: &Path) -> Option<Vec<u8>> {
    if !is_elf(filepath) {
        return None;
    }
    let data = fs::read(filepath).ok()?;
    if data.windows(12).any(|w| w == b"makeCWrapper")
        && data.windows(14).any(|w| w == b"set_env_prefix")
    {
        Some(data)
    } else {
        None
    }
}

struct WrapperEntry {
    action: String,  // prefix, suffix, set
    var: String,
    sep: String,
    value: String,
}

/// Parse the DOCSTRING embedded in a Nix compiled wrapper.
fn parse_wrapper_docstring(data: &[u8]) -> (String, Vec<WrapperEntry>) {
    // Clean binary: replace non-printable chars (except newlines) with spaces
    let text: String = data
        .iter()
        .map(|&b| {
            if b == b'\n' || (0x20..=0x7e).contains(&b) {
                b as char
            } else {
                ' '
            }
        })
        .collect();

    // (?s) enables DOTALL so `.` matches newlines in the multi-line docstring
    let re = Regex::new(r"(?s)makeCWrapper\s+'([^']+)'(.*?)#\s*\(Use").unwrap();
    let Some(caps) = re.captures(&text) else {
        return (String::new(), Vec::new());
    };

    let target = caps[1].to_string();
    let args_text = &caps[2];

    let entry_re =
        Regex::new(r"--(prefix|suffix|set)\s+'([^']+)'\s+'([^']+)'\s+'([^']+)'").unwrap();
    let entries: Vec<WrapperEntry> = entry_re
        .captures_iter(args_text)
        .map(|m| WrapperEntry {
            action: m[1].to_string(),
            var: m[2].to_string(),
            sep: m[3].to_string(),
            value: m[4].to_string(),
        })
        .collect();

    (target, entries)
}

/// Replace a compiled wrapper ELF with an equivalent shell script.
fn replace_nix_wrapper(
    filepath: &Path,
    data: &[u8],
    rewrite_map: &HashMap<String, String>,
    drop_hashes: &HashSet<String>,
) -> bool {
    let (target, entries) = parse_wrapper_docstring(data);
    if target.is_empty() {
        eprintln!(
            "WARNING: could not parse wrapper docstring: {}",
            filepath.display()
        );
        return false;
    }

    // Rewrite the target binary path
    let mut new_target = target.clone();
    let mut sorted_rewrites: Vec<(&str, &str)> = rewrite_map
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    sorted_rewrites.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

    for (old_path, new_path) in &sorted_rewrites {
        if target.starts_with(old_path) {
            new_target = format!("{new_path}{}", &target[old_path.len()..]);
            break;
        }
    }

    let nix_store_hash_re = Regex::new(r"^/nix/store/([a-z0-9]{32})-").unwrap();
    let qt_path_re = Regex::new(r"/usr/lib/qt-6/(plugins|qml)").unwrap();

    // Nixpkgs-specific env var renames — the runtime doesn't have the nixpkgs
    // patches that read these custom variable names.
    let env_renames: HashMap<&str, &str> = [
        ("NIXPKGS_QT6_QML_IMPORT_PATH", "QML_IMPORT_PATH"),
        ("NIXPKGS_QT5_QML_IMPORT_PATH", "QML2_IMPORT_PATH"),
        ("NIXPKGS_GST_PLUGIN_SYSTEM_PATH_1_0", "GST_PLUGIN_SYSTEM_PATH_1_0"),
    ]
    .into();

    // Group entries by env var, rewriting paths in values
    let mut env_ops: Vec<(String, Vec<(String, String, String)>)> = Vec::new();
    let mut env_ops_map: HashMap<String, usize> = HashMap::new();

    for entry in &entries {
        // Values may be compound (multiple paths joined by separator).
        // Split, rewrite each part individually, rejoin.
        let parts: Vec<&str> = if !entry.sep.is_empty() {
            entry.value.split(&entry.sep).collect()
        } else {
            vec![&entry.value]
        };

        let mut rewritten_parts = Vec::new();
        for part in &parts {
            let new_part = if let Some(caps) = nix_store_hash_re.captures(part) {
                let hash = &caps[1];
                if drop_hashes.contains(hash) {
                    // Dropped: map to /usr equivalent
                    let rel = Regex::new(r"^/nix/store/[^/]+/")
                        .unwrap()
                        .replace(part, "")
                        .to_string();
                    let usr_path = format!("/usr/{rel}");
                    // Fix Nix vs Flatpak layout: lib/qt-6/plugins → lib/plugins
                    qt_path_re.replace(&usr_path, "/usr/lib/$1").to_string()
                } else {
                    format!("/app{part}")
                }
            } else {
                part.to_string()
            };
            rewritten_parts.push(new_part);
        }

        let new_value = if !entry.sep.is_empty() {
            rewritten_parts.join(&entry.sep)
        } else {
            rewritten_parts.into_iter().next().unwrap_or_default()
        };

        // Apply env var renaming
        let var = env_renames
            .get(entry.var.as_str())
            .map_or_else(|| entry.var.clone(), |renamed| {
                eprintln!("  Renamed env var: {} -> {renamed}", entry.var);
                renamed.to_string()
            });

        if let Some(&idx) = env_ops_map.get(&var) {
            env_ops[idx]
                .1
                .push((entry.action.clone(), entry.sep.clone(), new_value));
        } else {
            let idx = env_ops.len();
            env_ops_map.insert(var.clone(), idx);
            env_ops.push((
                var,
                vec![(entry.action.clone(), entry.sep.clone(), new_value)],
            ));
        }
    }

    // Generate shell script
    let mut lines = vec!["#!/bin/sh".to_string()];

    for (var, ops) in &env_ops {
        // Deduplicate values
        let mut seen_values: HashSet<String> = HashSet::new();
        let mut unique_ops: Vec<(&str, &str, &str)> = Vec::new();
        for (action, sep, value) in ops {
            if seen_values.insert(value.clone()) {
                unique_ops.push((action, sep, value));
            }
        }

        let sep = unique_ops.first().map(|(_, s, _)| *s).unwrap_or(":");

        let prefix_values: Vec<&str> = unique_ops
            .iter()
            .filter(|(a, _, _)| *a == "prefix")
            .map(|(_, _, v)| *v)
            .collect();
        let suffix_values: Vec<&str> = unique_ops
            .iter()
            .filter(|(a, _, _)| *a == "suffix")
            .map(|(_, _, v)| *v)
            .collect();
        let set_values: Vec<&str> = unique_ops
            .iter()
            .filter(|(a, _, _)| *a == "set")
            .map(|(_, _, v)| *v)
            .collect();

        if let Some(last) = set_values.last() {
            lines.push(format!("export {var}=\"{last}\""));
        } else if !prefix_values.is_empty() && !suffix_values.is_empty() {
            let prefix_str = prefix_values.join(sep);
            let suffix_str = suffix_values.join(sep);
            lines.push(format!(
                "export {var}=\"{prefix_str}{sep}${{{var}:-}}{sep}{suffix_str}\""
            ));
        } else if !prefix_values.is_empty() {
            let prefix_str = prefix_values.join(sep);
            lines.push(format!("export {var}=\"{prefix_str}{sep}${{{var}:-}}\""));
        } else if !suffix_values.is_empty() {
            let suffix_str = suffix_values.join(sep);
            lines.push(format!("export {var}=\"${{{var}:-}}{sep}{suffix_str}\""));
        }
    }

    lines.push(format!("exec \"{new_target}\" \"$@\""));
    lines.push(String::new());

    let script_content = lines.join("\n");

    make_writable(filepath);
    if fs::write(filepath, &script_content).is_err() {
        return false;
    }
    let _ = fs::set_permissions(filepath, fs::Permissions::from_mode(0o755));

    eprintln!(
        "  Replaced compiled wrapper: {}",
        filepath.file_name().unwrap_or_default().to_string_lossy()
    );
    true
}

// ---------------------------------------------------------------------------
// Text file rewriting
// ---------------------------------------------------------------------------

fn rewrite_text_file(
    filepath: &Path,
    rewrite_map: &HashMap<String, String>,
    drop_hashes: &HashSet<String>,
) {
    if !is_text_file(filepath) {
        return;
    }

    let Ok(content) = fs::read_to_string(filepath) else {
        return;
    };
    if !content.contains("/nix/store/") {
        return;
    }

    let original = content.clone();
    let mut content = content;

    // Apply explicit rewrites (longest match first)
    let mut sorted_rewrites: Vec<(&str, &str)> = rewrite_map
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    sorted_rewrites.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

    for (old_path, new_path) in &sorted_rewrites {
        if content.contains(old_path) {
            content = content.replace(old_path, new_path);
        }
    }

    // Warn about remaining references to dropped paths
    let remaining_re = Regex::new(r"/nix/store/([a-z0-9]{32})-").unwrap();
    for caps in remaining_re.captures_iter(&content) {
        let hash = &caps[1];
        if drop_hashes.contains(hash) {
            eprintln!(
                "WARNING: {} still references dropped hash {hash}",
                filepath.display()
            );
        }
    }

    if content != original {
        let _ = fs::write(filepath, &content);
    }
}

// ---------------------------------------------------------------------------
// Desktop / D-Bus file rewriting
// ---------------------------------------------------------------------------

fn rewrite_desktop_file(filepath: &Path) {
    let Ok(content) = fs::read_to_string(filepath) else {
        return;
    };
    let original = content.clone();
    let nix_bin_re = Regex::new(r"/nix/store/[^/]+/bin/").unwrap();
    let app_bin_re = Regex::new(r"/app/nix/store/[^/]+/bin/").unwrap();

    let new_content: String = content
        .lines()
        .map(|line| {
            if let Some(rest) = line.strip_prefix("Exec=") {
                let cleaned = nix_bin_re.replace_all(rest, "");
                let cleaned = app_bin_re.replace_all(&cleaned, "");
                format!("Exec={cleaned}")
            } else if let Some(rest) = line.strip_prefix("TryExec=") {
                let cleaned = nix_bin_re.replace_all(rest, "");
                let cleaned = app_bin_re.replace_all(&cleaned, "");
                format!("TryExec={cleaned}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    if new_content != original {
        let _ = fs::write(filepath, &new_content);
    }
}

fn rewrite_dbus_service(filepath: &Path) {
    let Ok(content) = fs::read_to_string(filepath) else {
        return;
    };
    let original = content.clone();
    let nix_bin_re = Regex::new(r"/nix/store/[^/]+/bin/").unwrap();
    let app_nix_bin_re = Regex::new(r"/app/nix/store/[^/]+/bin/").unwrap();

    let new_content: String = content
        .lines()
        .map(|line| {
            if let Some(rest) = line.strip_prefix("Exec=") {
                let cleaned = nix_bin_re.replace_all(rest, "/app/bin/");
                let cleaned = app_nix_bin_re.replace_all(&cleaned, "/app/bin/");
                format!("Exec={cleaned}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    if new_content != original {
        let _ = fs::write(filepath, &new_content);
    }
}

// ---------------------------------------------------------------------------
// Runtime compatibility symlinks
// ---------------------------------------------------------------------------

/// Create symlinks for unversioned .so names that Nix binaries DT_NEED
/// but the runtime only provides as versioned SONAMEs.
fn create_runtime_compat_symlinks(
    output_dir: &Path,
    runtime_index: &Value,
    arch_triplet: &str,
) {
    let Some(runtime_sonames) = runtime_index["sonames"].as_object() else {
        return;
    };

    // Build map: unversioned name → versioned soname
    let unversioned_re = Regex::new(r"\.so\..*").unwrap();
    let mut unversioned_to_versioned: HashMap<String, String> = HashMap::new();
    for soname in runtime_sonames.keys() {
        let base = unversioned_re.replace(soname, ".so").to_string();
        if base != *soname && !runtime_sonames.contains_key(&base) {
            unversioned_to_versioned.insert(base, soname.clone());
        }
    }

    // Scan all ELF files for DT_NEEDED entries
    let mut needed_unversioned: HashSet<String> = HashSet::new();
    for entry in WalkDir::new(output_dir).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if !entry.file_type().is_file() {
            continue;
        }
        if path.symlink_metadata().map_or(true, |m| m.file_type().is_symlink()) {
            continue;
        }
        if !is_elf(path) {
            continue;
        }
        let Ok(data) = fs::read(path) else {
            continue;
        };
        let Ok(elf) = Elf::parse(&data) else {
            continue;
        };
        for lib in &elf.libraries {
            if unversioned_to_versioned.contains_key(*lib) {
                needed_unversioned.insert(lib.to_string());
            }
        }
    }

    if needed_unversioned.is_empty() {
        return;
    }

    let lib_dir = output_dir.join("lib");
    let _ = fs::create_dir_all(&lib_dir);

    let mut sorted: Vec<&String> = needed_unversioned.iter().collect();
    sorted.sort();
    for unversioned in sorted {
        let versioned = &unversioned_to_versioned[unversioned];
        let symlink_path = lib_dir.join(unversioned);
        let target = format!("/usr/lib/{arch_triplet}/{versioned}");
        if !symlink_path.exists() {
            let _ = std::os::unix::fs::symlink(&target, &symlink_path);
            eprintln!("  Compat symlink: {unversioned} -> {target}");
        }
    }
}

// ---------------------------------------------------------------------------
// Bin symlinks
// ---------------------------------------------------------------------------

fn create_bin_symlinks(output_dir: &Path, plan: &Value) {
    let bin_dir = output_dir.join("bin");
    let _ = fs::create_dir_all(&bin_dir);

    let Some(target_path) = plan["package"].as_str() else {
        return;
    };
    let target_basename = Path::new(target_path)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let target_bin = output_dir
        .join("nix/store")
        .join(target_basename.as_ref())
        .join("bin");

    if !target_bin.exists() {
        return;
    }

    let Ok(entries) = fs::read_dir(&target_bin) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() || path.symlink_metadata().map_or(false, |m| m.file_type().is_symlink())
        {
            let link = bin_dir.join(entry.file_name());
            if !link.exists() {
                // Create relative symlink
                if let Ok(rel) = pathdiff(&path, &bin_dir) {
                    let _ = std::os::unix::fs::symlink(&rel, &link);
                }
            }
        }
    }
}

/// Compute a relative path from `base` to `target`.
fn pathdiff(target: &Path, base: &Path) -> Result<PathBuf> {
    let target = fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    let base = fs::canonicalize(base).unwrap_or_else(|_| base.to_path_buf());

    let mut target_parts = target.components().peekable();
    let mut base_parts = base.components().peekable();

    // Skip common prefix
    while let (Some(t), Some(b)) = (target_parts.peek(), base_parts.peek()) {
        if t == b {
            target_parts.next();
            base_parts.next();
        } else {
            break;
        }
    }

    let mut result = PathBuf::new();
    for _ in base_parts {
        result.push("..");
    }
    for part in target_parts {
        result.push(part);
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let cli = Cli::parse();

    let plan: Value = serde_json::from_str(
        &fs::read_to_string(&cli.dedup_plan).context("reading dedup plan")?,
    )?;

    let output_dir = &cli.output_dir;
    fs::create_dir_all(output_dir)?;

    let rewrite_map = build_rewrite_map(&plan);
    let drop_hashes = build_drop_hashes(&plan);

    // Step 1: Copy kept store paths
    eprintln!("Copying store paths...");
    copy_store_paths(&plan, output_dir)?;

    // Step 2–4: Rewrite all files
    eprintln!("Rewriting files...");
    let mut elf_count = 0u32;
    let mut text_count = 0u32;
    let mut wrapper_count = 0u32;

    // Collect paths first to avoid borrowing issues with WalkDir while modifying files
    let all_files: Vec<PathBuf> = WalkDir::new(output_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .symlink_metadata()
                .map_or(false, |m| !m.file_type().is_symlink())
        })
        .map(|e| e.path().to_path_buf())
        .collect();

    for filepath in &all_files {
        if is_elf(filepath) {
            if let Some(wrapper_data) = check_nix_compiled_wrapper(filepath) {
                if replace_nix_wrapper(filepath, &wrapper_data, &rewrite_map, &drop_hashes) {
                    wrapper_count += 1;
                    continue;
                }
            }
            rewrite_elf(
                filepath,
                &rewrite_map,
                &drop_hashes,
                &cli.arch_triplet,
                &cli.patchelf,
            );
            elf_count += 1;
        } else if is_text_file(filepath) {
            rewrite_text_file(filepath, &rewrite_map, &drop_hashes);
            text_count += 1;

            let fname = filepath
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            if fname.ends_with(".desktop") {
                rewrite_desktop_file(filepath);
            } else if fname.ends_with(".service") && filepath.to_string_lossy().contains("dbus")
            {
                rewrite_dbus_service(filepath);
            }
        }
    }

    // Step 5: Create runtime compatibility symlinks
    if let Some(ri_path) = &cli.runtime_index {
        eprintln!("Creating runtime compat symlinks...");
        let runtime_index: Value =
            serde_json::from_str(&fs::read_to_string(ri_path)?)?;
        create_runtime_compat_symlinks(output_dir, &runtime_index, &cli.arch_triplet);
    }

    // Step 6: Create bin symlinks
    eprintln!("Creating bin symlinks...");
    create_bin_symlinks(output_dir, &plan);

    eprintln!(
        "Rewrote {elf_count} ELF files, {text_count} text files, {wrapper_count} compiled wrappers"
    );
    eprintln!("Done.");

    Ok(())
}
