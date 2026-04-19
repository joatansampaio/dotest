pub fn build_filter(input: Option<&str>) -> Option<String> {
    match input {
        Some(val) if val.is_empty() => None,
        Some(val) => {
            // If it already looks like a filter expression just pass it as is.
            if val.contains('=') || val.contains('~') || val.contains('&') || val.contains('|') {
                Some(val.to_string())
            } else {
                // Otherwise, treat as substring search on fully qualified name
                Some(format!("FullyQualifiedName~{}", val))
            }
        }
        None => None,
    }
}
