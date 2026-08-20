//! Fingerprint-based cache for `dotnet test -t` discovery. Skips discovery on startup when the
//! workspace looks unchanged since the last run.
//!
//! Prefer a **scoped** fingerprint over the test-project roots recorded in the cache (plus shallow
//! solution/config files). That way edits in unrelated app projects do not force a slow
//! rediscovery. Falls back to a full-tree walk only for legacy caches with no roots recorded.
//!
//! SHA-256 over discovery-relevant paths, sizes, and mtimes. Skips `bin`/`obj`/etc.

use crate::core::discovery::DiscoveredTest;
use crate::core::tree::TreeState;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Instant, UNIX_EPOCH};

pub(crate) const CACHE_PATH: &str = ".dotest_cache.json";

#[derive(Serialize, Deserialize)]
struct DiscoveryCacheFile {
    fingerprint: String,
    tests: Vec<DiscoveredTest>,
    #[serde(default)]
    tree_state: Option<TreeState>,
    /// Relative directories that contained test projects on the last successful discovery.
    /// When non-empty, fingerprinting is scoped to these roots (+ shallow solution/config).
    #[serde(default)]
    fingerprint_roots: Vec<String>,
}

/// In-process memo so try_load + save do not walk the tree twice.
static FINGERPRINT_MEMO: Mutex<Option<(Vec<String>, String)>> = Mutex::new(None);

pub(crate) fn discovery_log(msg: impl AsRef<str>) {
    eprintln!("[discovery] {}", msg.as_ref());
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
            hash_file_meta(&p, h);
        }
    }
    Ok(())
}

/// Shallow walk that only records solution/project/config files (not `.cs` sources).
fn hash_solution_and_config(path: &Path, depth: usize, max_depth: usize, h: &mut Sha256) -> io::Result<()> {
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
        if name_s == "." || name_s == ".." || name_s.starts_with('.') {
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
            hash_solution_and_config(&p, depth + 1, max_depth, h)?;
        } else if is_solution_or_config_file(&p) {
            hash_file_meta(&p, h);
        }
    }
    Ok(())
}

fn hash_file_meta(path: &Path, h: &mut Sha256) {
    h.update(path.to_string_lossy().as_bytes());
    h.update(&[0]);
    if let Ok(meta) = fs::metadata(path) {
        h.update(meta.len().to_le_bytes());
        if let Ok(m) = meta.modified() {
            if let Ok(d) = m.duration_since(UNIX_EPOCH) {
                h.update(d.as_secs().to_le_bytes());
                h.update(d.subsec_nanos().to_le_bytes());
            }
        }
    }
}

fn is_discovery_relevant_file(path: &Path) -> bool {
    is_solution_or_config_file(path)
        || matches!(
            path.extension()
                .and_then(|x| x.to_str())
                .map(|x| x.to_ascii_lowercase())
                .as_deref(),
            Some("cs")
        )
}

fn is_solution_or_config_file(path: &Path) -> bool {
    let file_name = path
        .file_name()
        .and_then(|x| x.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        file_name.as_str(),
        "global.json" | "nuget.config" | "packages.lock.json" | "directory.packages.props"
            | "directory.build.props" | "directory.build.targets"
    ) {
        return true;
    }

    matches!(
        path.extension()
            .and_then(|x| x.to_str())
            .map(|x| x.to_ascii_lowercase())
            .as_deref(),
        Some("csproj" | "sln" | "slnx" | "props" | "targets")
    )
}

fn filesystem_fingerprint_full() -> String {
    let mut h = Sha256::new();
    h.update(b"fs-v1\0");
    let _ = hash_dir(Path::new("."), 0, 10, &mut h);
    format!("{:x}", h.finalize())
}

fn filesystem_fingerprint_scoped(roots: &[String]) -> String {
    let mut h = Sha256::new();
    h.update(b"fs-v2-scoped\0");
    // Solution / Directory.Build.* / nuget config outside the test roots still matter.
    let _ = hash_solution_and_config(Path::new("."), 0, 3, &mut h);
    let mut sorted: Vec<&String> = roots.iter().collect();
    sorted.sort();
    for root in sorted {
        h.update(root.as_bytes());
        h.update(&[0]);
        let _ = hash_dir(Path::new(root), 0, 10, &mut h);
    }
    format!("{:x}", h.finalize())
}

fn memoized_fingerprint(roots: &[String]) -> String {
    if let Ok(guard) = FINGERPRINT_MEMO.lock() {
        if let Some((cached_roots, fp)) = guard.as_ref() {
            if cached_roots.as_slice() == roots {
                return fp.clone();
            }
        }
    }

    let started = Instant::now();
    let fp = if roots.is_empty() {
        discovery_log("fingerprint: full-tree walk (legacy / no scoped roots)");
        filesystem_fingerprint_full()
    } else {
        discovery_log(format!(
            "fingerprint: scoped walk over {} root(s): {}",
            roots.len(),
            roots.join(", ")
        ));
        filesystem_fingerprint_scoped(roots)
    };
    discovery_log(format!(
        "fingerprint: computed in {:.2}s ({})",
        started.elapsed().as_secs_f64(),
        if roots.is_empty() { "full" } else { "scoped" }
    ));

    if let Ok(mut guard) = FINGERPRINT_MEMO.lock() {
        *guard = Some((roots.to_vec(), fp.clone()));
    }
    fp
}

/// Derive relative directories to fingerprint from discovered test project targets.
pub(crate) fn fingerprint_roots_from_tests(tests: &[DiscoveredTest]) -> Vec<String> {
    let mut roots: BTreeSet<String> = BTreeSet::new();
    for test in tests {
        let Some(target) = test.target_path.as_deref() else {
            continue;
        };
        let path = Path::new(target);
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let rel = parent.to_string_lossy().replace('\\', "/");
        if rel.is_empty() || rel == "." {
            roots.insert(".".to_string());
        } else {
            roots.insert(rel);
        }
    }
    roots.into_iter().collect()
}

pub(crate) fn try_load_cached_tests() -> Option<Vec<DiscoveredTest>> {
    let started = Instant::now();
    let s = match fs::read_to_string(CACHE_PATH) {
        Ok(s) => s,
        Err(_) => {
            discovery_log("cache: miss (no .dotest_cache.json)");
            return None;
        }
    };
    let file: DiscoveryCacheFile = match serde_json::from_str(&s) {
        Ok(f) => f,
        Err(e) => {
            discovery_log(format!("cache: miss (invalid json: {e})"));
            return None;
        }
    };
    if file.tests.is_empty() {
        discovery_log("cache: miss (empty tests list)");
        return None;
    }

    let roots = file.fingerprint_roots.clone();
    let fp = memoized_fingerprint(&roots);
    if file.fingerprint == fp {
        discovery_log(format!(
            "cache: HIT ({} tests) in {:.2}s",
            file.tests.len(),
            started.elapsed().as_secs_f64()
        ));
        Some(file.tests)
    } else {
        discovery_log(format!(
            "cache: MISS (fingerprint mismatch, {} stored root(s)) in {:.2}s",
            roots.len(),
            started.elapsed().as_secs_f64()
        ));
        None
    }
}

pub(crate) fn save_discovery_cache(tests: &[DiscoveredTest]) -> Result<()> {
    if tests.is_empty() {
        return Ok(());
    }

    let started = Instant::now();
    let roots = fingerprint_roots_from_tests(tests);
    // Invalidate memo so we hash with the new roots (and after discovery files may have new mtimes).
    if let Ok(mut guard) = FINGERPRINT_MEMO.lock() {
        *guard = None;
    }
    let fp = memoized_fingerprint(&roots);
    let existing_state = load_tree_state();
    let file = DiscoveryCacheFile {
        fingerprint: fp,
        tests: tests.to_vec(),
        tree_state: existing_state,
        fingerprint_roots: roots.clone(),
    };
    let s = serde_json::to_string(&file).context("serialize discovery cache")?;
    fs::write(CACHE_PATH, s).context("write discovery cache")?;
    discovery_log(format!(
        "cache: saved {} tests (roots: {}) in {:.2}s",
        tests.len(),
        if roots.is_empty() {
            "(none)".to_string()
        } else {
            roots.join(", ")
        },
        started.elapsed().as_secs_f64()
    ));
    Ok(())
}

pub(crate) fn save_tree_state(state: TreeState) {
    let Ok(s) = fs::read_to_string(CACHE_PATH) else {
        return;
    };
    let Ok(mut file) = serde_json::from_str::<DiscoveryCacheFile>(&s) else {
        return;
    };
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

#[cfg(test)]
mod tests {
    use super::fingerprint_roots_from_tests;
    use crate::core::discovery::DiscoveredTest;

    #[test]
    fn fingerprint_roots_dedupe_project_parents() {
        let tests = vec![
            DiscoveredTest::new("A.T1", "Ns.A.T1", 1).with_target("tmly-test/tmly-test.csproj"),
            DiscoveredTest::new("B.T2", "Ns.B.T2", 1).with_target("tmly-test/tmly-test.csproj"),
            DiscoveredTest::new("C.T3", "Ns.C.T3", 1).with_target("other-tests/Other.csproj"),
        ];
        let roots = fingerprint_roots_from_tests(&tests);
        assert_eq!(roots, vec!["other-tests".to_string(), "tmly-test".to_string()]);
    }
}
