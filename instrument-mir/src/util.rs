pub(crate) fn prefixed_path(base: &str, prefix: &str) -> String {
    use std::path::{Path, PathBuf};

    let p = expand_tilde(base);
    let base_str = p.to_string_lossy();
    let ends_with_sep = base.ends_with(std::path::MAIN_SEPARATOR)
        || base.ends_with('/')
        || base.ends_with('\\');
    let is_dir = ends_with_sep || p.is_dir();

    let (parent, file_name) = if is_dir {
        (p, "out.mir".to_string())
    } else {
        let parent = p.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
        let file_name = p
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "out.mir".to_string());
        (parent, file_name)
    };

    let mut out: PathBuf = parent;
    out.push(format!("{}{}", prefix, file_name));
    out.to_string_lossy().to_string()
}

fn expand_tilde(path: &str) -> std::path::PathBuf {
    if path == "~" || path.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            if path == "~" {
                return std::path::PathBuf::from(home);
            }
            return std::path::PathBuf::from(home).join(&path[2..]);
        }
    }
    std::path::PathBuf::from(path)
}
