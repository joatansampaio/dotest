use anyhow::{bail, Context, Result};
use std::collections::{HashMap, BTreeMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use crate::core::config::Config;

static SELECTED_TEST_TARGET: OnceLock<Option<String>> = OnceLock::new();


/// Returns `(tree_fqn, filter_key, test_count)` triples.
/// `filter_key` is a VSTest-friendly substring for `FullyQualifiedName~` (typically `Namespace.Class.Method`).
/// `test_count` is the number of `dotnet test -t` lines for that logical test method.
pub fn discover_tests(no_build: bool, no_restore: bool) -> Result<Vec<(String, String, usize)>> {
    let display_names = discover_display_names(no_build, no_restore)?;
    if display_names.is_empty() {
        return Ok(Vec::new());
    }

    let test_roots = find_test_project_roots(Path::new("."));
    
    let mut method_map: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut class_map = HashMap::new();
    for root in &test_roots {
        let (m, c) = scan_source_maps(root);
        for (k, mut v) in m {
            method_map.entry(k).or_default().append(&mut v);
        }
        class_map.extend(c);
    }
    for v in method_map.values_mut() {
        v.sort_by(|a, b| a.1.cmp(&b.1));
    }

    Ok(build_discovery_entries(&display_names, &method_map, &class_map))
}

/// Maps each `dotnet test -t` line to a source method, merges counts, and builds vstest filter keys.
pub(crate) fn build_discovery_entries(
    display_names: &[String],
    methods: &HashMap<String, Vec<(String, String)>>,
    class_map: &HashMap<String, String>,
) -> Vec<(String, String, usize)> {
    let flat_for_enrich = flatten_methods_for_enrich(methods);

    // Group list indices by short method name (order within group: sorted display string)
    let mut by_short: BTreeMap<String, Vec<(usize, String)>> = BTreeMap::new();
    for (i, dn) in display_names.iter().enumerate() {
        let short = strip_params(dn);
        by_short.entry(short).or_default().push((i, dn.clone()));
    }

    let mut per_line: Vec<Option<(String, String)>> = vec![None; display_names.len()];

    for (short_and_class, mut group) in by_short {
        group.sort_by(|a, b| a.1.cmp(&b.1));
        
        // Try to match the display name (FQN) to a source method.
        // The display name from dotnet test -t is usually Namespace.Class.Method
        let parts: Vec<&str> = short_and_class.split('.').collect();
        let method_name = *parts.last().unwrap_or(&short_and_class.as_str());
        
        let cands = methods.get(method_name).cloned().unwrap_or_default();
        if cands.is_empty() {
            continue;
        }

        // Filter candidates to those where the qualified class matches the FQN prefix.
        let mut matching_cands = Vec::new();
        if parts.len() > 1 {
            let prefix = parts[..parts.len() - 1].join(".");
            for (folder, qualified_class) in &cands {
                if prefix == *qualified_class {
                    matching_cands.push((folder.clone(), qualified_class.clone()));
                }
            }
        }

        // Fallback: if no exact FQN match, but only one source method with this name, use it.
        // If multiple candidates exist and the input is not qualified, use all candidates for round-robin.
        let candidates_to_use = if !matching_cands.is_empty() {
            matching_cands
        } else if parts.len() == 1 || cands.len() == 1 {
            cands
        } else {
            // Truly ambiguous (e.g. FQN provided but prefix doesn't match any source class)
            continue;
        };

        let m = candidates_to_use.len();
        for (j, (i, _)) in group.iter().enumerate() {
            per_line[*i] = Some(candidates_to_use[j % m].clone());
        }
    }

    let mut out: Vec<(String, String, usize)> = Vec::new();
    let mut key_pos: HashMap<String, usize> = HashMap::new();

    for (i, dn) in display_names.iter().enumerate() {
        let short = strip_params(dn);
        let (tree_fqn, fk) = match &per_line[i] {
            Some((folder, qc)) => {
                let simple_method = short.rsplit('.').next().unwrap_or(&short);
                let fk = qualified_filter_key(qc, simple_method);
                let tree = tree_fqn_from_qualified(folder, qc, simple_method);
                (tree, fk)
            }
            None => {
                let tree = enrich(&short, &flat_for_enrich, class_map);
                (tree, short.clone())
            }
        };
        if let Some(&pos) = key_pos.get(&fk) {
            out[pos].2 += 1;
        } else {
            key_pos.insert(fk.clone(), out.len());
            out.push((tree_fqn, fk, 1));
        }
    }

    out
}

fn qualified_filter_key(qualified_class: &str, short_method: &str) -> String {
    format!("{}.{}", qualified_class, short_method)
}

/// Visual tree path: **on-disk folder** first (matches the test project layout in the IDE), then
/// class, then method. VSTest filters use `filter_key` (full qualified class + method) on leaves;
/// non-leaf filter strings are filled in by `build_flat_tree` after the flat list is built.
fn tree_fqn_from_qualified(folder: &str, qualified_class: &str, short_method: &str) -> String {
    let class_simple = qualified_class.rsplit('.').next().unwrap_or(qualified_class);
    if folder.is_empty() {
        format!("{}.{}", class_simple, short_method)
    } else {
        format!("{}.{}.{}", folder, class_simple, short_method)
    }
}

fn flatten_methods_for_enrich(
    methods: &HashMap<String, Vec<(String, String)>>,
) -> HashMap<String, (String, String)> {
    methods
        .iter()
        .filter_map(|(short, v)| {
            v.first().map(|(folder, qc)| {
                let cls_simple = qc.rsplit('.').next().unwrap_or(qc.as_str()).to_string();
                (short.clone(), (folder.clone(), cls_simple))
            })
        })
        .collect()
}

fn extract_namespace_declaration(line: &str) -> Option<String> {
    let t = line.trim_start();
    if !t.starts_with("namespace ") {
        return None;
    }
    let rest = t["namespace ".len()..].trim_start();
    let end = rest.find(|c| c == '{' || c == ';')?;
    let name = rest[..end].trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}



pub(crate) fn strip_params(s: &str) -> String {
    let mut res = String::new();
    let mut paren_depth: usize = 0;
    let mut angle_depth: usize = 0;
    let mut in_string = false;
    let mut escape = false;

    for c in s.chars() {
        if escape {
            escape = false;
            if paren_depth == 0 && angle_depth == 0 { res.push(c); }
            continue;
        }
        if c == '\\' {
            escape = true;
            if paren_depth == 0 && angle_depth == 0 { res.push(c); }
            continue;
        }
        if c == '"' {
            in_string = !in_string;
            if paren_depth == 0 && angle_depth == 0 { res.push(c); }
            continue;
        }

        if !in_string {
            if c == '(' { paren_depth += 1; continue; }
            if c == ')' { paren_depth = paren_depth.saturating_sub(1); continue; }
            if c == '<' { angle_depth += 1; continue; }
            if c == '>' { angle_depth = angle_depth.saturating_sub(1); continue; }
        }

        if paren_depth == 0 && angle_depth == 0 {
            res.push(c);
        }
    }
    res.trim().to_string()
}

/// Enrich a display‐name base into a `folder.class.method` tree path.
pub(crate) fn enrich(
    base: &str,
    method_map: &HashMap<String, (String, String)>,   // method -> (folder, class)
    class_map:  &HashMap<String, String>,              // class  -> folder
) -> String {
    let parts: Vec<&str> = base.split('.').collect();

    match parts.len() {
        0 => base.to_string(),

        1 => {
            let method = parts[0];
            if let Some((folder, class)) = method_map.get(method) {
                if folder.is_empty() {
                    format!("{}.{}", class, method)
                } else {
                    format!("{}.{}.{}", folder, class, method)
                }
            } else {
                // Maybe it's actually a class name with a single Test?
                if let Some(folder) = class_map.get(method) {
                    if folder.is_empty() {
                        method.to_string()
                    } else {
                        format!("{}.{}", folder, method)
                    }
                } else {
                    base.to_string()
                }
            }
        }

        // Multi‐segment -> e.g. "Namespace.ClassName.MethodName" or "Ns.Sub.Class.Method"
        // Scan segments to find a known class name and strip the namespace prefix.
        _ => {
            // Try to find a known class name among the segments.
            // Search backwards to find the most specific (likely last) class name.
            let mut class_idx: Option<usize> = None;
            for (i, &seg) in parts.iter().enumerate().rev() {
                // Strip generics before class lookup
                let base_seg = strip_params(seg);
                if class_map.contains_key(&base_seg) {
                    class_idx = Some(i);
                    break;
                }
            }

            if let Some(ci) = class_idx {
                let suffix = parts[ci..].join(".");
                let folder = &class_map[&strip_params(parts[ci])];
                if folder.is_empty() { suffix } else { format!("{}.{}", folder, suffix) }
            } else {
                let last = *parts.last().unwrap_or(&base);
                let last_base = strip_params(last);
                if let Some((folder, class)) = method_map.get(&last_base) {
                    // If the method map gives us a class name, try to find it in the parts.
                    if let Some(ci) = parts.iter().position(|&s| strip_params(s) == class.as_str()) {
                        let suffix = parts[ci..].join(".");
                        if folder.is_empty() { suffix } else { format!("{}.{}", folder, suffix) }
                    } else if !folder.is_empty() {
                        // If we can't find the class name but have a folder, at least prepend the folder.
                        // But first check if the folder is already a prefix to avoid duplication.
                        if base.starts_with(folder) {
                            base.to_string()
                        } else {
                            format!("{}.{}", folder, base)
                        }
                    } else {
                        base.to_string()
                    }
                } else {
                    base.to_string()
                }
            }
        }
    }
}

/// Returns (method_map, class_map):
///   method_map: short_method_name -> [(relative_folder, qualified_class_name), ...]
///   class_map:  simple class_name  -> relative_folder
fn scan_source_maps(root: &Path) -> (HashMap<String, Vec<(String, String)>>, HashMap<String, String>) {
    let mut method_map: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut class_map: HashMap<String, String> = HashMap::new();
    walk_cs(root, root, &mut method_map, &mut class_map, 0);
    (method_map, class_map)
}

fn walk_cs(
    root: &Path, dir: &Path,
    methods: &mut HashMap<String, Vec<(String, String)>>,
    classes: &mut HashMap<String, String>,
    depth: usize,
) {
    if depth > 10 { return; }
    let rd = match std::fs::read_dir(dir) { Ok(r) => r, Err(_) => return };
    for entry in rd.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if name.starts_with('.') || name == "obj" || name == "bin" { continue; }
            walk_cs(root, &path, methods, classes, depth + 1);
        } else if name.ends_with(".cs") {
            let rel = path.parent().unwrap_or(root);
            let rel = rel.strip_prefix(root).unwrap_or(rel);
            let dir_str = rel.to_string_lossy()
                .replace('\\', ".").replace('/', ".");
            let dir_str = dir_str.trim_matches('.').to_string();
            parse_cs_file(&path, &dir_str, methods, classes);
        }
    }
}

fn parse_cs_file(
    path: &Path, dir_str: &str,
    methods: &mut HashMap<String, Vec<(String, String)>>,
    classes: &mut HashMap<String, String>,
) {
    let content = match std::fs::read_to_string(path) { Ok(c) => c, Err(_) => return };
    parse_cs_content(&content, dir_str, methods, classes);
}

pub(crate) fn parse_cs_content(
    content: &str, dir_str: &str,
    methods: &mut HashMap<String, Vec<(String, String)>>,
    classes: &mut HashMap<String, String>,
) {
    // UTF-8 BOM (U+FEFF) breaks `namespace` on line 1 — common for VS-saved .cs files.
    let content = content.trim_start_matches('\u{feff}');
    let mut current_class: Option<String> = None;
    let mut has_test_attr = false;
    let mut namespace = String::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }

        if let Some(ns) = extract_namespace_declaration(trimmed) {
            namespace = ns;
            continue;
        }

        let stripped = strip_attributes(trimmed);

        if is_test_attribute(trimmed) {
            has_test_attr = true;
        }

        // Track class declarations
        if let Some(cls) = extract_class_name(stripped) {
            current_class = Some(cls.clone());
            classes.entry(cls).or_insert_with(|| dir_str.to_string());
            continue;
        }

        if has_test_attr {
            if stripped.is_empty() { continue; }
            // Skip comment-only remainders (e.g. [Test] // url  ->  stripped = "// url")
            if stripped.starts_with("//") { continue; }

            if let Some(method_name) = extract_method_name(stripped) {
                if let Some(ref cls) = current_class {
                    let qualified = if namespace.is_empty() {
                        cls.clone()
                    } else {
                        format!("{}.{}", namespace, cls)
                    };
                    methods.entry(method_name).or_default().push((dir_str.to_string(), qualified));
                }
            }
            has_test_attr = false;
        }
    }
}

pub(crate) fn strip_attributes(mut s: &str) -> &str {
    while s.starts_with('[') {
        let mut depth = 0;
        let mut end_idx = None;
        let mut in_string = false;
        let mut escape = false;

        for (i, c) in s.char_indices() {
             if escape {
                 escape = false;
                 continue;
             }
             if c == '\\' {
                 escape = true;
                 continue;
             }
             if c == '"' {
                 in_string = !in_string;
             } else if !in_string {
                 if c == '[' {
                     depth += 1;
                 } else if c == ']' {
                     depth -= 1;
                     if depth == 0 {
                         end_idx = Some(i);
                         break;
                     }
                 }
             }
        }
        
        if let Some(i) = end_idx {
            s = s[i + 1..].trim();
        } else {
            break; // Unmatched '['
        }
    }
    s
}

pub(crate) fn is_test_attribute(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with('[') || !trimmed.contains(']') { return false; }
    
    // Extract the content inside the FIRST set of brackets and trim it
    let end_bracket = trimmed.find(']').unwrap();
    let content = trimmed[1..end_bracket].trim();
    
    // Check for any known test attribute keywords (case-insensitive for robustness)
    let lower = content.to_lowercase();
    let keywords = [
        "test", "testcase", "testcasesource", "fact", "theory", 
        "datarow", "testmethod"
    ];
    
    for kw in keywords {
        // Match the keyword either as the whole word or followed by a comma/paren 
        // (to handle [Test, Category("Slow")] or [TestCase(1)])
        if lower == kw || lower.starts_with(&format!("{}(", kw)) || lower.starts_with(&format!("{} (", kw)) || lower.starts_with(&format!("{},", kw)) {
            return true;
        }
    }
    false
}

/// Extract method name from a method signature line.
/// e.g. `public void Foo()` -> `Foo`, `public async Task Bar(int x)` -> `Bar`
pub(crate) fn extract_method_name(line: &str) -> Option<String> {
    let paren_idx = line.find('(')?;
    let mut before = line[..paren_idx].trim();
    
    // Handle NUnit generic tests like MyTest<T>()
    if before.ends_with('>') {
        if let Some(angle_idx) = before.rfind('<') {
            before = before[..angle_idx].trim();
        }
    }

    // Take the last identifier before '(' or '<'
    let name: String = before.chars().rev()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect::<String>()
        .chars().rev().collect();

    if name.is_empty() { return None; }
    // Reject C# keywords that look like method calls
    let keywords = [
        "if", "while", "for", "foreach", "switch", "catch", "using",
        "lock", "class", "new", "throw", "return", "typeof", "sizeof",
        "nameof", "default", "await", "get", "set", "where",
    ];
    if keywords.contains(&name.as_str()) { None } else { Some(name) }
}

/// Extract class name from a `class X` declaration line.
pub(crate) fn extract_class_name(line: &str) -> Option<String> {
    let mut rest = line;
    loop {
        rest = rest.trim_start();
        let stripped = strip_modifier(rest);
        if stripped == rest { break; }
        rest = stripped;
    }
    rest = rest.trim_start();
    if rest.starts_with("class ") {
        let after = rest["class ".len()..].trim_start();
        let name: String = after.chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() { None } else { Some(name) }
    } else {
        None
    }
}

fn strip_modifier(s: &str) -> &str {
    for kw in &[
        "public", "internal", "protected", "private",
        "abstract", "sealed", "partial", "static", "readonly", "new", "virtual", "override",
    ] {
        if s.starts_with(kw) {
            let after = &s[kw.len()..];
            if after.starts_with(|c: char| c.is_whitespace()) { return after; }
        }
    }
    s
}

fn find_test_project_roots(root: &Path) -> Vec<std::path::PathBuf> {
    let mut results = Vec::new();
    collect_csproj(root, 0, &mut results);
    let mut test_roots = Vec::new();
    for csproj in results {
        if let Ok(content) = std::fs::read_to_string(&csproj) {
            let lower = content.to_lowercase();
            if lower.contains("nunit") || lower.contains("xunit") || lower.contains("mstest") {
                if let Some(p) = csproj.parent() {
                    test_roots.push(p.to_path_buf());
                }
            }
        }
    }
    test_roots
}

fn collect_csproj(dir: &Path, depth: usize, out: &mut Vec<std::path::PathBuf>) {
    if depth > 5 { return; }
    let rd = match std::fs::read_dir(dir) { Ok(r) => r, Err(_) => return };
    for entry in rd.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if name.starts_with('.') || name == "obj" || name == "bin" { continue; }
            collect_csproj(&path, depth + 1, out);
        } else if name.ends_with(".csproj") {
            out.push(path);
        }
    }
}

fn is_solution_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("sln") | Some("slnx")
    )
}

fn is_project_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("csproj")
}

fn is_test_project_file(path: &Path) -> bool {
    if !is_project_file(path) {
        return false;
    }
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let lower = content.to_lowercase();
    lower.contains("microsoft.net.test.sdk")
        || lower.contains("<istestproject>true</istestproject>")
        || lower.contains("xunit")
        || lower.contains("nunit")
        || lower.contains("mstest")
}

fn collect_solution_or_project_in_dir(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if is_solution_file(&path) || is_project_file(&path) {
            out.push(path);
        }
    } 
    out.sort();
    out
}

fn collect_solution_and_project_files(
    dir: &Path,
    depth: usize,
    out: &mut Vec<PathBuf>,
) {
    if depth > 5 {
        return;
    }

    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };

    for entry in rd.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if name.starts_with('.') || name == "obj" || name == "bin" || name == "target" {
                continue;
            }
            collect_solution_and_project_files(&path, depth + 1, out);
        } else if path.is_file() && (is_solution_file(&path) || is_project_file(&path)) {
            out.push(path);
        }
    }
}

fn prompt_user_for_target(candidates: &[PathBuf], cwd: &Path) -> Result<Option<String>> {
    if candidates.is_empty() {
        return Ok(None);
    }

    if candidates.len() == 1 {
        let rel = candidates[0]
            .strip_prefix(cwd)
            .unwrap_or(&candidates[0])
            .to_string_lossy()
            .replace('\\', "/");
        println!("Using discovered test target: {}", rel);
        return Ok(Some(rel));
    }

    println!();
    println!("Multiple solution/project targets were discovered.");
    println!("Select a target to run `dotnet test` against:");
    for (idx, p) in candidates.iter().enumerate() {
        let rel = p.strip_prefix(cwd).unwrap_or(p).to_string_lossy().replace('\\', "/");
        println!("  {}. {}", idx + 1, rel);
    }
    println!();

    loop {
        print!("Enter number (1-{}), or press Enter to cancel: ", candidates.len());
        let mut stdout = std::io::stdout();
        let _ = std::io::Write::flush(&mut stdout);

        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .context("Failed to read target selection")?;
        let trimmed = input.trim();

        if trimmed.is_empty() {
            return Ok(None);
        }

        if let Ok(n) = trimmed.parse::<usize>() {
            if n >= 1 && n <= candidates.len() {
                let rel = candidates[n - 1]
                    .strip_prefix(cwd)
                    .unwrap_or(&candidates[n - 1])
                    .to_string_lossy()
                    .replace('\\', "/");
                println!("Selected test target: {}", rel);
                return Ok(Some(rel));
            }
        }

        println!("Invalid selection. Please type a number between 1 and {}.", candidates.len());
    }
}

fn resolve_test_target(no_prompt: bool) -> Result<Option<String>> {
    if let Some(cached) = SELECTED_TEST_TARGET.get() {
        return Ok(cached.clone());
    }

    let cwd = std::env::current_dir().context("Failed to get current directory")?;
    let direct_candidates = collect_solution_or_project_in_dir(&cwd);
    let direct_solutions: Vec<PathBuf> = direct_candidates
        .iter()
        .filter(|p| is_solution_file(p))
        .cloned()
        .collect();
    let direct_test_projects: Vec<PathBuf> = direct_candidates
        .iter()
        .filter(|p| is_test_project_file(p))
        .cloned()
        .collect();

    if direct_solutions.len() == 1 {
        let rel = direct_solutions[0]
            .strip_prefix(&cwd)
            .unwrap_or(&direct_solutions[0])
            .to_string_lossy()
            .replace('\\', "/");
        let target = Some(rel);
        let _ = SELECTED_TEST_TARGET.set(target.clone());
        return Ok(target);
    }

    if direct_solutions.len() > 1 {
        let target = if no_prompt {
            direct_solutions
                .first()
                .map(|p| p.strip_prefix(&cwd).unwrap_or(p).to_string_lossy().replace('\\', "/"))
        } else {
            prompt_user_for_target(&direct_solutions, &cwd)?
        };
        let _ = SELECTED_TEST_TARGET.set(target.clone());
        return Ok(target);
    }

    if direct_test_projects.len() == 1 {
        let rel = direct_test_projects[0]
            .strip_prefix(&cwd)
            .unwrap_or(&direct_test_projects[0])
            .to_string_lossy()
            .replace('\\', "/");
        let target = Some(rel);
        let _ = SELECTED_TEST_TARGET.set(target.clone());
        return Ok(target);
    }

    if direct_test_projects.len() > 1 {
        let target = if no_prompt {
            direct_test_projects
                .first()
                .map(|p| p.strip_prefix(&cwd).unwrap_or(p).to_string_lossy().replace('\\', "/"))
        } else {
            prompt_user_for_target(&direct_test_projects, &cwd)?
        };
        let _ = SELECTED_TEST_TARGET.set(target.clone());
        return Ok(target);
    }

    if direct_candidates.len() > 1 {
        let target = if no_prompt {
            direct_candidates
                .first()
                .map(|p| p.strip_prefix(&cwd).unwrap_or(p).to_string_lossy().replace('\\', "/"))
        } else {
            prompt_user_for_target(&direct_candidates, &cwd)?
        };
        let _ = SELECTED_TEST_TARGET.set(target.clone());
        return Ok(target);
    }

    let mut candidates = Vec::new();
    collect_solution_and_project_files(&cwd, 0, &mut candidates);
    candidates.sort_by(|a, b| {
        let a_sln = is_solution_file(a);
        let b_sln = is_solution_file(b);
        b_sln.cmp(&a_sln).then_with(|| a.cmp(b))
    });

    let target = if no_prompt {
        candidates
            .first()
            .map(|p| p.strip_prefix(&cwd).unwrap_or(p).to_string_lossy().replace('\\', "/"))
    } else {
        prompt_user_for_target(&candidates, &cwd)?
    };
    let _ = SELECTED_TEST_TARGET.set(target.clone());
    Ok(target)
}

fn discover_display_names(no_build: bool, no_restore: bool) -> Result<Vec<String>> {
    let target = resolve_test_target(false)?;
    let mut cmd = Command::new("dotnet");
    cmd.arg("test")
        .arg("/p:UseSharedCompilation=true")
        .arg("/p:BaseOutputPath=bin/dotest/")
        .arg("-t");
    if let Some(target) = &target {
        cmd.arg(target);
    }
    if no_build { cmd.arg("--no-build"); }
    if no_restore { cmd.arg("--no-restore"); }
    let output = cmd.output().context("Failed to run dotnet test -t")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut tests = Vec::new();
    let mut capturing = false;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed == "The following Tests are available:" { capturing = true; continue; }
        
        if capturing && !trimmed.is_empty() {
            let lower = trimmed.to_lowercase();
            // Skip obvious summary/footer lines (some SDKs append these after the list)
            let is_garbage = lower.starts_with("total tests:")
                || lower.starts_with("passed:")
                || lower.starts_with("failed:")
                || lower.starts_with("skipped:");
            if !is_garbage {
                tests.push(trimmed.to_string());
            }
        }
    }

    // Some repos emit partial build failures/warnings but still print a valid test list.
    // If discovery produced tests, continue instead of failing on a non-zero exit code.
    if !output.status.success() && tests.is_empty() {
        bail!(
            "{}",
            format_discovery_failure(
                output.status.code(),
                &stdout,
                &stderr,
                no_build,
                no_restore,
                target.as_deref(),
            )
        );
    }
    Ok(tests)
}

pub(crate) fn format_discovery_failure(
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
    no_build: bool,
    no_restore: bool,
    target: Option<&str>,
) -> String {
    let mut command = "dotnet test /p:UseSharedCompilation=true /p:BaseOutputPath=bin/dotest/ -t".to_string();
    if let Some(target) = target {
        command.push(' ');
        command.push_str(target);
    }
    if no_build {
        command.push_str(" --no-build");
    }
    if no_restore {
        command.push_str(" --no-restore");
    }

    let mut message = format!(
        "Test discovery failed while running `{}`{}.",
        command,
        exit_code.map_or_else(String::new, |code| format!(" (exit code {code})"))
    );

    let combined = format!("{stdout}\n{stderr}");
    if combined.contains("A compatible .NET SDK was not found")
        || combined.contains("Requested SDK version:")
        || combined.contains("global.json file:")
    {
        message.push_str(
            "\n\nThe .NET SDK selected by global.json is not installed or cannot be used. \
Install the requested SDK, or update global.json to an SDK installed on this machine. \
Run `dotnet --list-sdks` to see what is available.",
        );
    }
    if combined.contains("MSBUILD : error MSB1003")
        || combined.contains("Specify a project or solution file.")
    {
        message.push_str(
            "\n\nNo `.sln` or `.csproj` file was found in the current directory. \
Run dotest from a solution/project folder, or choose a discovered target when prompted.",
        );
    }
    if combined.contains("MSBUILD : error MSB1011")
        || combined.contains("Specify which project or solution file to use")
    {
        message.push_str(
            "\n\nMultiple `.sln`/`.csproj` files were found in this directory. \
Select a specific target from the prompt menu.",
        );
    }
    if combined.contains("The test source file")
        && combined.contains("provided was not found")
        && no_build
    {
        message.push_str(
            "\n\nThe selected target has not been built yet, but discovery was run with `--no-build`. \
Disable \"Skip build\" for discovery/run or run a normal `dotnet test` once to produce test binaries.",
        );
    }
    if combined.contains("error MSB3202")
        && combined.contains("project file")
        && combined.contains("was not found")
    {
        message.push_str(
            "\n\nThe selected solution references local sibling repositories/projects that are missing on this machine. \
Choose another solution/project target, or clone the missing dependency repos.",
        );
    }

    if !stderr.trim().is_empty() {
        message.push_str("\n\nstderr:\n");
        message.push_str(stderr.trim());
    }

    if !stdout.trim().is_empty() {
        message.push_str("\n\nstdout:\n");
        message.push_str(stdout.trim());
    }

    message
}



/// Build a `dotnet test` Command with filter and config exclusions applied.
/// Caller controls Stdio (piped vs inherited).
pub fn build_test_command(filter: Option<String>, no_build: bool, no_restore: bool) -> Command {
    let mut cmd = Command::new("dotnet");
    cmd.arg("test")
        .arg("/p:UseSharedCompilation=true")
        .arg("/p:BaseOutputPath=bin/dotest/");
    if let Ok(Some(target)) = resolve_test_target(true) {
        cmd.arg(target);
    }

    let mut final_filter = filter;
    if let Ok(config) = Config::new() {
        if let Ok(settings) = config.load_settings() {
            if !settings.excluded_categories.is_empty() {
                let excludes: Vec<String> = settings.excluded_categories.iter()
                    .map(|c| format!("Category!={}", c))
                    .collect();
                let exclude_str = excludes.join("&");
                match final_filter {
                    Some(f) => final_filter = Some(format!("({})&({})", f, exclude_str)),
                    None    => final_filter = Some(exclude_str),
                }
            }
        }
    }

    if let Some(f) = final_filter { cmd.arg("--filter"); cmd.arg(f); }
    if no_build { cmd.arg("--no-build"); }
    if no_restore { cmd.arg("--no-restore"); }
    cmd
}
