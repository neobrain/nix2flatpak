//! Index the contents of a Flatpak runtime for use by nix2flatpak.
//!
//! Walks a runtime's `files/` directory and catalogues every shared library
//! (with its SONAME), executable, and data file category.  The resulting
//! JSON index is consumed by `nix2flatpak-analyze-closure` to decide which
//! Nix store paths can be deduplicated against the runtime.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Parser;
use regex::Regex;
use serde_json::{json, Map, Value};
use walkdir::WalkDir;

use nix2flatpak::{extract_soname, is_elf, is_script};

#[derive(Parser)]
#[command(about = "Index a Flatpak runtime for nix2flatpak")]
struct Cli {
    /// Path to the Flatpak runtime's files/ directory
    runtime_path: PathBuf,
    #[arg(long, short)]
    output: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Library indexing
// ---------------------------------------------------------------------------

fn index_libraries(files_dir: &Path) -> BTreeMap<String, Value> {
    let mut sonames: BTreeMap<String, Value> = BTreeMap::new();
    let lib_dir = files_dir.join("lib");
    if !lib_dir.exists() {
        return sonames;
    }

    for entry in WalkDir::new(&lib_dir).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        let fname = entry.file_name().to_string_lossy();
        if !fname.contains(".so") {
            continue;
        }

        // Skip symlinks (only process actual files)
        if let Ok(meta) = path.symlink_metadata() {
            if meta.file_type().is_symlink() {
                // But if target doesn't exist in our walk, still process
                if let Ok(target) = path.canonicalize() {
                    if target.exists() && target != path {
                        continue;
                    }
                }
            }
        }

        if !path.is_file() {
            continue;
        }

        if let Some(soname) = extract_soname(path) {
            if !sonames.contains_key(&soname) {
                if let Ok(rel) = path.strip_prefix(files_dir) {
                    sonames.insert(
                        soname,
                        json!({"path": rel.to_string_lossy()}),
                    );
                }
            }
        }
    }
    sonames
}

// ---------------------------------------------------------------------------
// Executable indexing
// ---------------------------------------------------------------------------

fn index_executables(files_dir: &Path) -> BTreeMap<String, Value> {
    let mut executables: BTreeMap<String, Value> = BTreeMap::new();

    for subdir in ["bin", "libexec", "sbin"] {
        let exec_dir = files_dir.join(subdir);
        if !exec_dir.exists() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&exec_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            // Check executable bit
            if let Ok(meta) = path.metadata() {
                if meta.permissions().mode() & 0o100 == 0 {
                    continue;
                }
            }

            let name = entry.file_name().to_string_lossy().to_string();
            let rel_path = path
                .strip_prefix(files_dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            let file_type = if is_elf(&path) {
                "elf"
            } else if is_script(&path) {
                "script"
            } else {
                "other"
            };
            executables.insert(name, json!({"path": rel_path, "type": file_type}));
        }
    }
    executables
}

// ---------------------------------------------------------------------------
// Data indexing
// ---------------------------------------------------------------------------

fn index_data(files_dir: &Path) -> Map<String, Value> {
    let mut data = Map::new();

    // GLib schemas
    let schemas_dir = files_dir.join("share/glib-2.0/schemas");
    if schemas_dir.exists() {
        if let Ok(entries) = fs::read_dir(&schemas_dir) {
            let schemas: BTreeSet<String> = entries
                .flatten()
                .filter(|e| {
                    e.path().is_file()
                        && e.path().extension().map_or(false, |ext| ext == "xml")
                })
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            data.insert(
                "glib-schemas".into(),
                json!(schemas.into_iter().collect::<Vec<_>>()),
            );
        }
    }

    // GIR typelibs
    let typelib_dirs: Vec<PathBuf> = glob_dirs(files_dir, "lib/**/girepository-1.0");
    if !typelib_dirs.is_empty() {
        let mut typelibs: BTreeSet<String> = BTreeSet::new();
        for td in &typelib_dirs {
            if let Ok(entries) = fs::read_dir(td) {
                for e in entries.flatten() {
                    if e.path().extension().map_or(false, |ext| ext == "typelib") {
                        typelibs.insert(e.file_name().to_string_lossy().to_string());
                    }
                }
            }
        }
        data.insert(
            "typelibs".into(),
            json!(typelibs.into_iter().collect::<Vec<_>>()),
        );
    }

    // Locale directories
    let locale_dir = files_dir.join("share/locale");
    if locale_dir.exists() {
        if let Ok(entries) = fs::read_dir(&locale_dir) {
            let locales: BTreeSet<String> = entries
                .flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            data.insert(
                "locale".into(),
                json!(locales.into_iter().collect::<Vec<_>>()),
            );
        }
    }

    // Icon themes
    let icons_dir = files_dir.join("share/icons");
    if icons_dir.exists() {
        if let Ok(entries) = fs::read_dir(&icons_dir) {
            let themes: BTreeSet<String> = entries
                .flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            data.insert(
                "icon-themes".into(),
                json!(themes.into_iter().collect::<Vec<_>>()),
            );
        }
    }

    // MIME database
    data.insert("mime".into(), json!(files_dir.join("share/mime").exists()));

    // Python
    let python_dirs = glob_dirs(files_dir, "lib/python3.*");
    let python_dirs = if python_dirs.is_empty() {
        glob_dirs(files_dir, "lib/*/python3.*")
    } else {
        python_dirs
    };
    if let Some(py_dir) = python_dirs.first() {
        let version = py_dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .replace("python", "");
        let modules: Vec<String> = if py_dir.exists() {
            let mut mods: Vec<String> = fs::read_dir(py_dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.path().is_dir() && !e.file_name().to_string_lossy().starts_with("__"))
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            mods.sort();
            mods
        } else {
            Vec::new()
        };
        data.insert("python".into(), json!({"version": version, "modules": modules}));
    }

    // Qt6 plugins
    let qt_plugin_dirs = glob_dirs(files_dir, "lib/**/qt6/plugins");
    if !qt_plugin_dirs.is_empty() {
        let mut plugins: BTreeSet<String> = BTreeSet::new();
        for qd in &qt_plugin_dirs {
            for entry in WalkDir::new(qd).into_iter().filter_map(|e| e.ok()) {
                let fname = entry.file_name().to_string_lossy();
                if fname.ends_with(".so") {
                    plugins.insert(fname.to_string());
                }
            }
        }
        data.insert(
            "qt-plugins".into(),
            json!(plugins.into_iter().collect::<Vec<_>>()),
        );
    }

    // GStreamer plugins
    let gst_dirs = glob_dirs(files_dir, "lib/**/gstreamer-1.0");
    if !gst_dirs.is_empty() {
        let mut plugins: BTreeSet<String> = BTreeSet::new();
        for gd in &gst_dirs {
            if let Ok(entries) = fs::read_dir(gd) {
                for e in entries.flatten() {
                    if e.path().is_file()
                        && e.path().extension().map_or(false, |ext| ext == "so")
                    {
                        plugins.insert(e.file_name().to_string_lossy().to_string());
                    }
                }
            }
        }
        data.insert(
            "gstreamer-plugins".into(),
            json!(plugins.into_iter().collect::<Vec<_>>()),
        );
    }

    data
}

/// Simple glob for `base/pattern` — supports one `**` and one `*`.
fn glob_dirs(base: &Path, pattern: &str) -> Vec<PathBuf> {
    // Split pattern into segments
    let parts: Vec<&str> = pattern.split('/').collect();
    let mut results = vec![base.to_path_buf()];

    for part in parts {
        let mut next = Vec::new();
        for dir in &results {
            if part == "**" {
                // Recursive: collect all subdirectories
                for entry in WalkDir::new(dir)
                    .min_depth(0)
                    .into_iter()
                    .filter_map(|e| e.ok())
                {
                    if entry.file_type().is_dir() {
                        next.push(entry.path().to_path_buf());
                    }
                }
            } else if part.contains('*') {
                // Wildcard match
                let re_pattern = format!("^{}$", part.replace('.', r"\.").replace('*', ".*"));
                let re = Regex::new(&re_pattern).unwrap();
                if let Ok(entries) = fs::read_dir(dir) {
                    for e in entries.flatten() {
                        let name = e.file_name().to_string_lossy().to_string();
                        if re.is_match(&name) && e.path().is_dir() {
                            next.push(e.path());
                        }
                    }
                }
            } else {
                let candidate = dir.join(part);
                if candidate.exists() {
                    next.push(candidate);
                }
            }
        }
        results = next;
    }
    results.sort();
    results
}

// ---------------------------------------------------------------------------
// Metadata parsing
// ---------------------------------------------------------------------------

/// Parse a simple INI-format metadata file.
fn parse_metadata(files_dir: &Path) -> Option<Map<String, Value>> {
    let metadata_path = files_dir.parent()?.join("metadata");
    let content = fs::read_to_string(&metadata_path).ok()?;

    let mut result: Map<String, Value> = Map::new();
    let mut current_section: Option<String> = None;
    let mut section_map: Map<String, Value> = Map::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            // Save previous section
            if let Some(ref section) = current_section {
                result.insert(section.clone(), Value::Object(section_map.clone()));
            }
            current_section = Some(line[1..line.len() - 1].to_string());
            section_map = Map::new();
        } else if let Some(eq_pos) = line.find('=') {
            let key = line[..eq_pos].trim().to_string();
            let value = line[eq_pos + 1..].trim().to_string();
            section_map.insert(key, json!(value));
        }
    }
    // Save last section
    if let Some(section) = current_section {
        result.insert(section, Value::Object(section_map));
    }

    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

/// Parse manifest.json for CPE product information.
fn parse_manifest(files_dir: &Path) -> Option<Vec<Value>> {
    let manifest_path = files_dir.join("manifest.json");
    let content = fs::read_to_string(&manifest_path).ok()?;
    let raw: Value = serde_json::from_str(&content).ok()?;

    let items = if let Some(arr) = raw.as_array() {
        arr.clone()
    } else if let Some(modules) = raw.get("modules").and_then(|m| m.as_array()) {
        modules.clone()
    } else if let Some(components) = raw.get("components").and_then(|c| c.as_array()) {
        components.clone()
    } else {
        return None;
    };

    let mut products = Vec::new();
    for item in &items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let mut entry = Map::new();
        if let Some(cpe) = obj.get("x-cpe").and_then(|c| c.as_object()) {
            entry.insert(
                "name".into(),
                json!(cpe
                    .get("product")
                    .and_then(|p| p.as_str())
                    .or_else(|| obj.get("name").and_then(|n| n.as_str()))
                    .unwrap_or("")),
            );
            entry.insert(
                "version".into(),
                json!(cpe.get("version").and_then(|v| v.as_str()).unwrap_or("")),
            );
        } else if let Some(name) = obj.get("name").and_then(|n| n.as_str()) {
            entry.insert("name".into(), json!(name));
            entry.insert(
                "version".into(),
                json!(obj.get("version").and_then(|v| v.as_str()).unwrap_or("")),
            );
        }
        if entry.get("name").and_then(|n| n.as_str()).map_or(true, |n| n.is_empty()) {
            continue;
        }
        products.push(Value::Object(entry));
    }

    if products.is_empty() {
        None
    } else {
        Some(products)
    }
}

// ---------------------------------------------------------------------------
// Version extraction
// ---------------------------------------------------------------------------

/// Extract glibc version from libc.so.6 by scanning for GLIBC_x.y version strings.
fn extract_glibc_version(files_dir: &Path, sonames: &BTreeMap<String, Value>) -> Option<String> {
    let libc_info = sonames.get("libc.so.6")?;
    let libc_rel = libc_info["path"].as_str()?;
    let libc_path = files_dir.join(libc_rel);
    let data = fs::read(&libc_path).ok()?;

    let re = Regex::new(r"GLIBC_(\d+)\.(\d+)").unwrap();
    let text = String::from_utf8_lossy(&data);
    let mut max_version: (u32, u32) = (0, 0);

    for caps in re.captures_iter(&text) {
        let major: u32 = caps[1].parse().ok()?;
        let minor: u32 = caps[2].parse().ok()?;
        if (major, minor) > max_version {
            max_version = (major, minor);
        }
    }

    if max_version != (0, 0) {
        Some(format!("{}.{}", max_version.0, max_version.1))
    } else {
        None
    }
}

/// Extract versions for ABI-critical libraries.
fn extract_key_versions(
    files_dir: &Path,
    sonames: &BTreeMap<String, Value>,
) -> Map<String, Value> {
    let mut versions = Map::new();

    if let Some(glibc_ver) = extract_glibc_version(files_dir, sonames) {
        versions.insert("glibc".into(), json!(glibc_ver));
    }

    // libstdc++: version from filename like libstdc++.so.6.0.34
    if let Some(info) = sonames.get("libstdc++.so.6") {
        if let Some(path) = info["path"].as_str() {
            let re = Regex::new(r"libstdc\+\+\.so\.(\d+\.\d+\.\d+)").unwrap();
            if let Some(caps) = re.captures(path) {
                versions.insert("libstdcxx".into(), json!(&caps[1]));
            }
        }
    }

    // Qt6: version from libQt6Core.so.6.x.y filename
    if let Some(info) = sonames.get("libQt6Core.so.6") {
        if let Some(path) = info["path"].as_str() {
            let re = Regex::new(r"libQt6Core\.so\.(\d+\.\d+\.\d+)").unwrap();
            if let Some(caps) = re.captures(path) {
                versions.insert("qt".into(), json!(&caps[1]));
            }
        }
    }

    versions
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let cli = Cli::parse();

    let files_dir = &cli.runtime_path;
    anyhow::ensure!(files_dir.exists(), "{} does not exist", files_dir.display());

    let sonames = index_libraries(files_dir);
    let executables = index_executables(files_dir);
    let data = index_data(files_dir);

    let mut index = Map::new();
    // Emit sonames as a JSON object
    let sonames_obj: Map<String, Value> = sonames
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    index.insert("sonames".into(), Value::Object(sonames_obj));
    index.insert(
        "executables".into(),
        json!(executables),
    );
    index.insert("data".into(), Value::Object(data));

    let versions = extract_key_versions(files_dir, &sonames);
    if !versions.is_empty() {
        index.insert("versions".into(), Value::Object(versions));
    }

    if let Some(metadata) = parse_metadata(files_dir) {
        index.insert("metadata".into(), Value::Object(metadata));
    }

    if let Some(manifest) = parse_manifest(files_dir) {
        index.insert("manifest".into(), json!(manifest));
    }

    let output = serde_json::to_string_pretty(&Value::Object(index))?;

    if let Some(output_path) = &cli.output {
        fs::write(output_path, format!("{output}\n"))?;
    } else {
        println!("{output}");
    }

    Ok(())
}
