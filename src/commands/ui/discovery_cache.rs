//! Fingerprint-based cache for `dotnet test -t` discovery.
//!
//! A matching fingerprint reuses the last correct test list and skips `dotnet test -t`.
//! When only some test projects changed, those projects are rediscovered and the rest of
//! the cached list is kept. A solution / `Directory.Build.*` / NuGet config change, or a
//! legacy cache with no per-project fingerprints, falls back to a full discovery.
//!
//! SHA-256 over discovery-relevant paths, sizes, and mtimes. Skips `bin`/`obj`/etc.

use crate::core::discovery::{discovery_log, DiscoveredTest};
use crate::core::tree::TreeState;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Instant, UNIX_EPOCH};

/// Above this many changed projects, one solution-level `dotnet test -t` is faster than
/// several project builds that each recompile the same dependencies.
const MAX_INCREMENTAL_PROJECTS: usize = 3;

pub(crate) const CACHE_PATH: &str = ".dotest_cache.json";

/// Bump when discovery output shape changes so a stale list (for example a root
/// `Validate` leaf) is not reused after the parser fix.
const DISCOVERY_CACHE_VERSION: u32 = 2;

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
    /// Shallow solution / Directory.Build.* / NuGet config fingerprint.
    /// Empty on caches written before per-project baselines existed.
    #[serde(default)]
    solution_fingerprint: String,
    /// Per test-project content fingerprint, keyed by relative `.csproj` path.
    #[serde(default)]
    project_fingerprints: BTreeMap<String, String>,
    #[serde(default)]
    discovery_version: u32,
}

/// Previous correct discovery, detailed enough to refresh only what changed.
pub(crate) struct DiscoveryBaseline {
    pub tests: Vec<DiscoveredTest>,
    pub solution_fingerprint: String,
    pub project_fingerprints: BTreeMap<String, String>,
}

pub(crate) enum LoadedCache {
    /// Per-project baseline from a previous correct discovery.
    Baseline(DiscoveryBaseline),
    /// Older cache: one overall fingerprint, no per-project split.
    Legacy {
        tests: Vec<DiscoveredTest>,
        fingerprint: String,
        roots: Vec<String>,
    },
    Missing,
}

#[derive(Debug)]
pub(crate) enum IncrementalPlan {
    /// Every stored project still matches. Reuse `tests` as-is.
    Unchanged,
    /// `keep` is still correct. `refresh` is the csproj paths to rediscover.
    /// An empty `refresh` means projects were only removed.
    Partial {
        keep: Vec<DiscoveredTest>,
        refresh: Vec<String>,
    },
    /// Baseline cannot be spliced safely; run a full discovery.
    Full,
}

/// In-process memo so try_load + save do not walk the tree twice.
static FINGERPRINT_MEMO: Mutex<Option<(Vec<String>, String)>> = Mutex::new(None);

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
fn hash_solution_and_config(
    path: &Path,
    depth: usize,
    max_depth: usize,
    h: &mut Sha256,
) -> io::Result<()> {
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
        "global.json"
            | "nuget.config"
            | "packages.lock.json"
            | "directory.packages.props"
            | "directory.build.props"
            | "directory.build.targets"
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

pub(crate) fn solution_fingerprint() -> String {
    let mut h = Sha256::new();
    h.update(b"sln-v1\0");
    let _ = hash_solution_and_config(Path::new("."), 0, 3, &mut h);
    format!("{:x}", h.finalize())
}

pub(crate) fn project_fingerprint(target_path: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"proj-v1\0");
    let parent = Path::new(target_path)
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let _ = hash_dir(parent, 0, 10, &mut h);
    format!("{:x}", h.finalize())
}

fn project_fingerprints_from_tests(
    tests: &[DiscoveredTest],
    also_track: &[String],
) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for test in tests {
        let Some(target) = test.target_path.as_deref() else {
            continue;
        };
        if target.is_empty() {
            continue;
        }
        map.entry(target.to_string())
            .or_insert_with(|| project_fingerprint(target));
    }
    // Keep watching projects that rediscovered to an empty test list, so the next edit
    // still refreshes them instead of looking unchanged.
    for target in also_track {
        if target.is_empty() || !Path::new(target).is_file() {
            continue;
        }
        map.entry(target.clone())
            .or_insert_with(|| project_fingerprint(target));
    }
    map
}

pub(crate) fn has_anchored_tests(tests: &[DiscoveredTest]) -> bool {
    tests.iter().any(|test| {
        test.target_path
            .as_deref()
            .is_some_and(|path| !path.is_empty())
    })
}

/// Fingerprints for stored projects whose `.csproj` is still on disk.
/// A missing file is omitted so the planner treats that project as removed.
pub(crate) fn live_fingerprints(stored: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let started = Instant::now();
    let mut live = BTreeMap::new();
    for target in stored.keys() {
        if !Path::new(target).is_file() {
            continue;
        }
        live.insert(target.clone(), project_fingerprint(target));
    }
    discovery_log(format!(
        "baseline: hashed {}/{} project(s) in {:.2}s",
        live.len(),
        stored.len(),
        started.elapsed().as_secs_f64()
    ));
    live
}

pub(crate) fn load_cache() -> LoadedCache {
    let s = match fs::read_to_string(CACHE_PATH) {
        Ok(s) => s,
        Err(_) => {
            discovery_log("cache: miss (no .dotest_cache.json)");
            return LoadedCache::Missing;
        }
    };
    let file: DiscoveryCacheFile = match serde_json::from_str(&s) {
        Ok(f) => f,
        Err(e) => {
            discovery_log(format!("cache: miss (invalid json: {e})"));
            return LoadedCache::Missing;
        }
    };
    if file.tests.is_empty() {
        discovery_log("cache: miss (empty tests list)");
        return LoadedCache::Missing;
    }
    if file.discovery_version != DISCOVERY_CACHE_VERSION {
        discovery_log(format!(
            "cache: miss (discovery version {} != {DISCOVERY_CACHE_VERSION})",
            file.discovery_version
        ));
        return LoadedCache::Missing;
    }
    if file.solution_fingerprint.is_empty() || file.project_fingerprints.is_empty() {
        return LoadedCache::Legacy {
            tests: file.tests,
            fingerprint: file.fingerprint,
            roots: file.fingerprint_roots,
        };
    }
    LoadedCache::Baseline(DiscoveryBaseline {
        tests: file.tests,
        solution_fingerprint: file.solution_fingerprint,
        project_fingerprints: file.project_fingerprints,
    })
}

/// Decide what to reuse from the last correct discovery.
///
/// `live` must contain a fingerprint for every stored project whose csproj still exists,
/// and nothing else. Missing keys are treated as deleted projects.
pub(crate) fn plan_incremental(
    tests: &[DiscoveredTest],
    stored: &BTreeMap<String, String>,
    live: &BTreeMap<String, String>,
) -> IncrementalPlan {
    if stored.is_empty() || tests.is_empty() {
        return IncrementalPlan::Full;
    }

    let mut refresh = Vec::new();
    let mut unchanged: BTreeSet<String> = BTreeSet::new();
    for (target, stored_fp) in stored {
        match live.get(target) {
            None => {}
            Some(fp) if fp == stored_fp => {
                unchanged.insert(target.clone());
            }
            Some(_) => refresh.push(target.clone()),
        }
    }
    for target in live.keys() {
        if !stored.contains_key(target) {
            refresh.push(target.clone());
        }
    }
    refresh.sort();
    refresh.dedup();

    if refresh.is_empty() && unchanged.len() == stored.len() {
        return IncrementalPlan::Unchanged;
    }

    // One solution build beats several project builds once most of the suite moved,
    // or when nothing stable is left to reuse and more than one project must rebuild.
    if refresh.len() > MAX_INCREMENTAL_PROJECTS || (unchanged.is_empty() && refresh.len() > 1) {
        return IncrementalPlan::Full;
    }

    let keep: Vec<_> = tests
        .iter()
        .filter(|test| match test.target_path.as_deref() {
            Some(path) if !path.is_empty() => unchanged.contains(path),
            // Tests we could not tie to a project stay in the baseline. A refreshed
            // project replaces them when it emits the same filter key.
            _ => true,
        })
        .cloned()
        .collect();

    IncrementalPlan::Partial { keep, refresh }
}

pub(crate) fn merge_discovered(
    keep: Vec<DiscoveredTest>,
    refreshed: Vec<DiscoveredTest>,
) -> Vec<DiscoveredTest> {
    let replaced: std::collections::HashSet<&str> = refreshed
        .iter()
        .map(|test| test.filter_key.as_str())
        .collect();
    let mut out: Vec<_> = keep
        .into_iter()
        .filter(|test| !replaced.contains(test.filter_key.as_str()))
        .collect();
    out.extend(refreshed);
    out.sort_by(|a, b| {
        a.tree_path
            .cmp(&b.tree_path)
            .then(a.filter_key.cmp(&b.filter_key))
    });
    out
}

pub(crate) fn legacy_fingerprint_matches(fingerprint: &str, roots: &[String]) -> bool {
    memoized_fingerprint(roots) == fingerprint
}

pub(crate) fn save_discovery_cache(tests: &[DiscoveredTest]) -> Result<()> {
    save_discovery_cache_tracking(tests, &[])
}

/// `also_track` are csproj paths rediscovered in this run, including ones that now have no tests.
pub(crate) fn save_discovery_cache_tracking(
    tests: &[DiscoveredTest],
    also_track: &[String],
) -> Result<()> {
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
    // Record a per-project baseline from tests that name a project. Tests with no
    // project are still stored and reused; projects that never matched a test are not
    // watched until a full discovery can attach them.
    let (solution_fingerprint, project_fingerprints) = if has_anchored_tests(tests) {
        (
            solution_fingerprint(),
            project_fingerprints_from_tests(tests, also_track),
        )
    } else {
        (String::new(), BTreeMap::new())
    };
    let file = DiscoveryCacheFile {
        fingerprint: fp,
        tests: tests.to_vec(),
        tree_state: existing_state,
        fingerprint_roots: roots.clone(),
        solution_fingerprint,
        project_fingerprints,
        discovery_version: DISCOVERY_CACHE_VERSION,
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
    use super::{
        fingerprint_roots_from_tests, merge_discovered, plan_incremental, IncrementalPlan,
    };
    use crate::core::discovery::DiscoveredTest;
    use std::collections::BTreeMap;

    fn fps(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn fingerprint_roots_dedupe_project_parents() {
        let tests = vec![
            DiscoveredTest::new("A.T1", "Ns.A.T1", 1).with_target("tmly-test/tmly-test.csproj"),
            DiscoveredTest::new("B.T2", "Ns.B.T2", 1).with_target("tmly-test/tmly-test.csproj"),
            DiscoveredTest::new("C.T3", "Ns.C.T3", 1).with_target("other-tests/Other.csproj"),
        ];
        let roots = fingerprint_roots_from_tests(&tests);
        assert_eq!(
            roots,
            vec!["other-tests".to_string(), "tmly-test".to_string()]
        );
    }

    #[test]
    fn unchanged_projects_reuse_cached_tests() {
        let tests = vec![
            DiscoveredTest::new("A.T", "A.T", 1).with_target("a/a.csproj"),
            DiscoveredTest::new("B.T", "B.T", 2).with_target("b/b.csproj"),
        ];
        let stored = fps(&[("a/a.csproj", "fp-a"), ("b/b.csproj", "fp-b")]);
        let plan = plan_incremental(&tests, &stored, &stored);
        assert!(matches!(plan, IncrementalPlan::Unchanged));
    }

    #[test]
    fn changed_project_keeps_the_other_projects_tests() {
        let tests = vec![
            DiscoveredTest::new("A.T", "A.T", 1).with_target("a/a.csproj"),
            DiscoveredTest::new("B.T", "B.T", 2).with_target("b/b.csproj"),
            DiscoveredTest::new("B.U", "B.U", 1).with_target("b/b.csproj"),
        ];
        let stored = fps(&[("a/a.csproj", "fp-a"), ("b/b.csproj", "fp-b")]);
        let live = fps(&[("a/a.csproj", "fp-a"), ("b/b.csproj", "fp-b-new")]);
        match plan_incremental(&tests, &stored, &live) {
            IncrementalPlan::Partial { keep, refresh } => {
                assert_eq!(keep.len(), 1);
                assert_eq!(keep[0].filter_key, "A.T");
                assert_eq!(refresh, vec!["b/b.csproj".to_string()]);
            }
            other => panic!("expected partial refresh, got {other:?}"),
        }
    }

    #[test]
    fn deleted_project_drops_its_tests_without_a_rebuild_list() {
        let tests = vec![
            DiscoveredTest::new("A.T", "A.T", 1).with_target("a/a.csproj"),
            DiscoveredTest::new("B.T", "B.T", 1).with_target("b/b.csproj"),
        ];
        let stored = fps(&[("a/a.csproj", "fp-a"), ("b/b.csproj", "fp-b")]);
        let live = fps(&[("a/a.csproj", "fp-a")]);
        match plan_incremental(&tests, &stored, &live) {
            IncrementalPlan::Partial { keep, refresh } => {
                assert!(refresh.is_empty());
                assert_eq!(keep.len(), 1);
                assert_eq!(keep[0].filter_key, "A.T");
            }
            other => panic!("expected deletion-only plan, got {other:?}"),
        }
    }

    #[test]
    fn unanchored_tests_stay_while_a_changed_project_refreshes() {
        let tests = vec![
            DiscoveredTest::new("A.T", "A.T", 1).with_target("a/a.csproj"),
            DiscoveredTest::new("Loose.T", "Loose.T", 1),
        ];
        let stored = fps(&[("a/a.csproj", "fp-a"), ("b/b.csproj", "fp-b")]);
        let live = fps(&[("a/a.csproj", "fp-a"), ("b/b.csproj", "fp-b-new")]);
        match plan_incremental(&tests, &stored, &live) {
            IncrementalPlan::Partial { keep, refresh } => {
                assert_eq!(refresh, vec!["b/b.csproj".to_string()]);
                let keys: Vec<_> = keep.iter().map(|test| test.filter_key.as_str()).collect();
                assert_eq!(keys, vec!["A.T", "Loose.T"]);
            }
            other => panic!("expected partial refresh, got {other:?}"),
        }
    }

    #[test]
    fn single_changed_project_refreshes_even_with_nothing_else_to_keep() {
        let tests = vec![DiscoveredTest::new("A.T", "A.T", 1).with_target("a/a.csproj")];
        let stored = fps(&[("a/a.csproj", "old")]);
        let live = fps(&[("a/a.csproj", "new")]);
        match plan_incremental(&tests, &stored, &live) {
            IncrementalPlan::Partial { keep, refresh } => {
                assert!(keep.is_empty());
                assert_eq!(refresh, vec!["a/a.csproj".to_string()]);
            }
            other => panic!("expected single-project refresh, got {other:?}"),
        }
    }

    #[test]
    fn matching_fingerprints_stay_unchanged_even_with_unanchored_tests() {
        let tests = vec![
            DiscoveredTest::new("A.T", "A.T", 1).with_target("a/a.csproj"),
            DiscoveredTest::new("Loose.T", "Loose.T", 1),
        ];
        let stored = fps(&[("a/a.csproj", "fp-a")]);
        assert!(matches!(
            plan_incremental(&tests, &stored, &stored),
            IncrementalPlan::Unchanged
        ));
    }

    #[test]
    fn too_many_changed_projects_uses_one_full_discovery() {
        let tests = vec![
            DiscoveredTest::new("A.T", "A.T", 1).with_target("a/a.csproj"),
            DiscoveredTest::new("B.T", "B.T", 1).with_target("b/b.csproj"),
            DiscoveredTest::new("C.T", "C.T", 1).with_target("c/c.csproj"),
            DiscoveredTest::new("D.T", "D.T", 1).with_target("d/d.csproj"),
            DiscoveredTest::new("E.T", "E.T", 1).with_target("e/e.csproj"),
        ];
        let stored = fps(&[
            ("a/a.csproj", "a"),
            ("b/b.csproj", "b"),
            ("c/c.csproj", "c"),
            ("d/d.csproj", "d"),
            ("e/e.csproj", "e"),
        ]);
        let live = fps(&[
            ("a/a.csproj", "a"),
            ("b/b.csproj", "b2"),
            ("c/c.csproj", "c2"),
            ("d/d.csproj", "d2"),
            ("e/e.csproj", "e2"),
        ]);
        assert!(matches!(
            plan_incremental(&tests, &stored, &live),
            IncrementalPlan::Full
        ));
    }

    #[test]
    fn merge_prefers_refreshed_filter_key_and_sorts() {
        let keep = vec![
            DiscoveredTest::new("Z.Same", "Shared.Key", 1).with_target("a/a.csproj"),
            DiscoveredTest::new("A.Keep", "A.Keep", 1).with_target("a/a.csproj"),
        ];
        let refreshed = vec![
            DiscoveredTest::new("M.Same", "Shared.Key", 4).with_target("b/b.csproj"),
            DiscoveredTest::new("B.New", "B.New", 3).with_target("b/b.csproj"),
        ];
        let merged = merge_discovered(keep, refreshed);
        let keys: Vec<_> = merged.iter().map(|test| test.filter_key.as_str()).collect();
        assert_eq!(keys, vec!["A.Keep", "B.New", "Shared.Key"]);
        let shared = merged
            .iter()
            .find(|test| test.filter_key == "Shared.Key")
            .unwrap();
        assert_eq!(shared.test_count, 4);
        assert_eq!(shared.tree_path, "M.Same");
    }
}
