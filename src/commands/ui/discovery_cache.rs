//! Fingerprint-based cache for `dotnet test -t` discovery. Skips discovery on startup when the
//! workspace looks unchanged since the last run.
//!
//! SHA-256 over discovery-relevant source/config paths, sizes, and mtimes (shallow walk,
//! skips `bin`/`obj`/etc.). Generated files and unrelated git noise should not force a
//! slow `dotnet test -t` rediscovery.

use crate::core::discovery::DiscoveredTest;
use crate::core::tree::TreeState;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::path::Path;
use std::time::UNIX_EPOCH;

pub(crate) const CACHE_PATH: &str = ".dotest_cache.json";

#[derive(Serialize, Deserialize)]
struct DiscoveryCacheFile {
    fingerprint: String,
    tests: Vec<DiscoveredTest>,
    #[serde(default)]
    tree_state: Option<TreeState>,
}

fn hash_dir(path: &Path, depth: usize, max_depth: usize, h: &mut Sha256) -> io::Result<()> {
    if depth > max_depth {
        return Ok(());
    }
    let rd = match fs::read_dir(path) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.path());
    for e in entries {
        let name = e.file_name();
        let name_s = name.to_string_lossy();
        if name_s == "." || name_s == ".." {
            continue;
        }
        if name_s.starts_with('.') {
            continue;
        }
        if matches!(
            name_s.as_ref(),
            "bin" | "obj" | "target" | "node_modules" | "packages"
        ) {
            continue;
        }
        let p = e.path();
        if p.is_dir() {
            hash_dir(&p, depth + 1, max_depth, h)?;
        } else if is_discovery_relevant_file(&p) {
            h.update(p.to_string_lossy().as_bytes());
            h.update(&[0]);
            if let Ok(meta) = fs::metadata(&p) {
                h.update(meta.len().to_le_bytes());
                if let Ok(m) = meta.modified() {
                    if let Ok(d) = m.duration_since(UNIX_EPOCH) {
                        h.update(d.as_secs().to_le_bytes());
                        h.update(d.subsec_nanos().to_le_bytes());
                    }
                }
            }
        }
    }
    Ok(())
}

fn is_discovery_relevant_file(path: &Path) -> bool {
    let file_name = path
        .file_name()
        .and_then(|x| x.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        file_name.as_str(),
        "global.json" | "nuget.config" | "packages.lock.json" | "directory.packages.props"
    ) {
        return true;
    }

    matches!(
        path.extension()
            .and_then(|x| x.to_str())
            .map(|x| x.to_ascii_lowercase())
            .as_deref(),
        Some("cs" | "csproj" | "sln" | "slnx" | "props" | "targets")
    )
}

fn filesystem_fingerprint() -> String {
    let mut h = Sha256::new();
    h.update(b"fs-v1\0");
    let _ = hash_dir(Path::new("."), 0, 10, &mut h);
    format!("{:x}", h.finalize())
}

pub(crate) fn compute_source_fingerprint() -> String {
    filesystem_fingerprint()
}

pub(crate) fn try_load_cached_tests() -> Option<Vec<DiscoveredTest>> {
    let fp = compute_source_fingerprint();
    let s = fs::read_to_string(CACHE_PATH).ok()?;
    let file: DiscoveryCacheFile = serde_json::from_str(&s).ok()?;
    if file.fingerprint == fp && !file.tests.is_empty() {
        Some(file.tests)
    } else {
        None
    }
}

pub(crate) fn save_discovery_cache(tests: &[DiscoveredTest]) -> Result<()> {
    if tests.is_empty() {
        return Ok(());
    }

    let fp = compute_source_fingerprint();
    // Preserve any existing tree_state when rewriting the cache.
    let existing_state = load_tree_state();
    let file = DiscoveryCacheFile {
        fingerprint: fp,
        tests: tests.to_vec(),
        tree_state: existing_state,
    };
    let s = serde_json::to_string(&file).context("serialize discovery cache")?;
    fs::write(CACHE_PATH, s).context("write discovery cache")?;
    Ok(())
}

pub(crate) fn save_tree_state(state: TreeState) {
    let Ok(s) = fs::read_to_string(CACHE_PATH) else { return; };
    let Ok(mut file) = serde_json::from_str::<DiscoveryCacheFile>(&s) else { return; };
    file.tree_state = Some(state);
    if let Ok(out) = serde_json::to_string(&file) {
        let _ = fs::write(CACHE_PATH, out);
    }
}

pub(crate) fn load_tree_state() -> Option<TreeState> {
    let s = fs::read_to_string(CACHE_PATH).ok()?;
    let file: DiscoveryCacheFile = serde_json::from_str(&s).ok()?;
    file.tree_state
}
