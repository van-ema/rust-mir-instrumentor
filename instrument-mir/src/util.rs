pub(crate) fn prefixed_path(base: &str, prefix: &str) -> String {
    use std::path::{Path, PathBuf};

    let p = Path::new(base);
    let parent = p.parent().unwrap_or_else(|| Path::new(""));
    let file_name = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "mir.txt".to_string());

    let mut out: PathBuf = parent.to_path_buf();
    out.push(format!("{}{}", prefix, file_name));
    out.to_string_lossy().to_string()
}
