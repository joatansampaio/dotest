use anyhow::{Context, Result};
use std::process::Command;
use std::path::Path;
use std::collections::HashMap;
use crate::core::config::Config;



/// Returns `(tree_fqn, filter_key)` pairs.
pub fn discover_tests(no_build: bool) -> Result<Vec<(String, String)>> {
    let display_names = discover_display_names(no_build)?;
    if display_names.is_empty() {
        return Ok(Vec::new());
    }

    let test_root = find_test_project_root(Path::new("."));
    let scan_root = test_root.as_deref().unwrap_or(Path::new("."));
    let (method_map, class_map) = scan_source_maps(scan_root);

    let mut result: Vec<(String, String)> = Vec::new();
    for dn in &display_names {
        let filter_key = strip_params(dn);
        let tree_fqn = enrich(&filter_key, &method_map, &class_map);
        if !result.iter().any(|(_, k)| k == &filter_key) {
            result.push((tree_fqn, filter_key));
        }
    }

    Ok(result)
}



pub(crate) fn strip_params(s: &str) -> String {
    if let Some(p) = s.find('(') { s[..p].to_string() } else { s.to_string() }
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

        // Multi‐segment -> e.g. "ClassName.MethodName" or "Outer.Inner.Method"
        // Try to look up first segment as a class to get its folder prefix.
        _ => {
            let first = parts[0];
            if let Some(folder) = class_map.get(first) {
                if folder.is_empty() {
                    base.to_string()
                } else {
                    format!("{}.{}", folder, base)
                }
            } else {
                // Fallback: try looking up the LAST segment as a method
                let last = *parts.last().unwrap();
                if let Some((folder, _class)) = method_map.get(last) {
                    if !folder.is_empty() {
                        // Only prepend folder if the first segment isn't already a directory segment
                        // that's part of the enrichment
                        format!("{}.{}", folder, base)
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
///   method_map: method_name -> (relative_folder, class_name)
///   class_map:  class_name  -> relative_folder
fn scan_source_maps(root: &Path) -> (HashMap<String, (String, String)>, HashMap<String, String>) {
    let mut method_map: HashMap<String, (String, String)> = HashMap::new();
    let mut class_map: HashMap<String, String> = HashMap::new();
    walk_cs(root, root, &mut method_map, &mut class_map, 0);
    (method_map, class_map)
}

fn walk_cs(
    root: &Path, dir: &Path,
    methods: &mut HashMap<String, (String, String)>,
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
    methods: &mut HashMap<String, (String, String)>,
    classes: &mut HashMap<String, String>,
) {
    let content = match std::fs::read_to_string(path) { Ok(c) => c, Err(_) => return };
    parse_cs_content(&content, dir_str, methods, classes);
}

pub(crate) fn parse_cs_content(
    content: &str, dir_str: &str,
    methods: &mut HashMap<String, (String, String)>,
    classes: &mut HashMap<String, String>,
) {
    let mut current_class: Option<String> = None;
    let mut has_test_attr = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }

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

            if let Some(method_name) = extract_method_name(stripped) {
                if let Some(ref cls) = current_class {
                    methods.entry(method_name)
                        .or_insert_with(|| (dir_str.to_string(), cls.clone()));
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
    line.starts_with("[Test]")
        || line.starts_with("[Test(")
        || line.starts_with("[TestCase")
        || line.starts_with("[Theory")
        || line.starts_with("[Fact")
        || line.starts_with("[TestMethod")
        || line.starts_with("[Test,")
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

fn find_test_project_root(root: &Path) -> Option<std::path::PathBuf> {
    let mut results = Vec::new();
    collect_csproj(root, 0, &mut results);
    for csproj in &results {
        if let Ok(content) = std::fs::read_to_string(csproj) {
            if content.contains("NUnit") || content.contains("xunit")
                || content.contains("MSTest") || content.contains("nunit") {
                return csproj.parent().map(|p| p.to_path_buf());
            }
        }
    }
    None
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

fn discover_display_names(no_build: bool) -> Result<Vec<String>> {
    let mut cmd = Command::new("dotnet");
    cmd.arg("test").arg("-t");
    if no_build { cmd.arg("--no-build"); }
    let output = cmd.output().context("Failed to run dotnet test -t")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut tests = Vec::new();
    let mut capturing = false;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed == "The following Tests are available:" { capturing = true; continue; }
        if capturing && !trimmed.is_empty() { tests.push(trimmed.to_string()); }
    }
    Ok(tests)
}



/// Build a `dotnet test` Command with filter and config exclusions applied.
/// Caller controls Stdio (piped vs inherited).
pub fn build_test_command(filter: Option<String>, no_build: bool) -> Command {
    let mut cmd = Command::new("dotnet");
    cmd.arg("test");

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
    cmd
}
