//! Backend-agnostic shader compile coordination: cache keys, cache dir,
//! async compile queue, and shared budget constants.
//!
//! Modeled on the proven D3D11 flow in os/windows/d3d11.rs. The skeleton never
//! understands artifact contents — it only moves `Vec<u8>` blobs and a backend
//! `u64` cache key. DXBC / MTLBinaryArchive / GL program-binary / SPIR-V
//! differences stay encapsulated in each backend.

/// FNV-1a 64-bit of `source`, seeded with `backend_version`. Bump the
/// backend's CACHE_KEY_VERSION when compile flags / entry points / artifact
/// format change, so stale on-disk artifacts from an older engine are
/// invalidated instead of being read back as mismatched bytes.
pub fn shader_cache_key(source: &str, backend_version: u8) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    hash ^= backend_version as u64;
    hash = hash.wrapping_mul(0x100000001b3);
    for byte in source.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Per-user cache directory `<cache>/makepad/<subdir>`, created if missing.
/// Returns None if no cache dir is resolvable (caller then skips disk cache).
pub fn shader_cache_dir(subdir: &str) -> Option<std::path::PathBuf> {
    let base = dirs_cache_dir()?;
    let path = base.join("makepad").join(subdir);
    std::fs::create_dir_all(&path).ok()?;
    Some(path)
}

// Resolve the OS cache dir without adding a dependency: macOS uses
// ~/Library/Caches, other unix uses $XDG_CACHE_HOME or ~/.cache. Windows
// backends resolve their own dir via SHGetKnownFolderPath (see d3d11.rs) and
// do not call this helper.
#[allow(unreachable_code)]
fn dirs_cache_dir() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")?;
        return Some(std::path::PathBuf::from(home).join("Library").join("Caches"));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(x) = std::env::var_os("XDG_CACHE_HOME") {
            return Some(std::path::PathBuf::from(x));
        }
        let home = std::env::var_os("HOME")?;
        return Some(std::path::PathBuf::from(home).join(".cache"));
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_is_stable_and_version_sensitive() {
        let a = shader_cache_key("fragment_main() {}", 1);
        let b = shader_cache_key("fragment_main() {}", 1);
        assert_eq!(a, b, "same source+version must hash identically");

        let c = shader_cache_key("fragment_main() {}", 2);
        assert_ne!(a, c, "bumping backend_version must change the key");

        let d = shader_cache_key("fragment_main() {} // edit", 1);
        assert_ne!(a, d, "changed source must change the key");
    }
}
