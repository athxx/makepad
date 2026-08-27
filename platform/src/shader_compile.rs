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

use std::collections::{HashSet, VecDeque};
use std::sync::mpsc;
use std::sync::Mutex;

pub const SHADER_CREATE_BUDGET: usize = 4;

/// Completion of one background compile. Carries only status — the artifact
/// bytes were written to the on-disk cache by the worker and are re-read on
/// the main thread, so the channel never ferries large blobs.
pub struct AsyncCompileResult {
    pub shader_id: usize,
    pub status: Result<(), String>,
}

struct Inner {
    tx: mpsc::Sender<AsyncCompileResult>,
    rx: mpsc::Receiver<AsyncCompileResult>,
    pending: HashSet<usize>,
    ready_backlog: VecDeque<AsyncCompileResult>,
}

/// Background compile queue. Generic over a *thread-safe* `compile_fn`: a
/// backend passes a fn that produces serializable artifact bytes without
/// touching its GPU context (Metal must NOT use this for pipeline creation —
/// see the Metal backend for the main-thread constraint).
pub struct AsyncShaderCompile {
    inner: Mutex<Inner>,
}

impl AsyncShaderCompile {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            inner: Mutex::new(Inner {
                tx,
                rx,
                pending: HashSet::new(),
                ready_backlog: VecDeque::new(),
            }),
        }
    }

    /// Start a background compile for `shader_id`. No-op (returns false) if that
    /// id is already in flight. The worker writes the artifact to
    /// `<cache_dir>/<cache_key:016x>.<artifact_ext>` (skipping empty artifacts,
    /// so a backend that manages its own files can return `Vec::new()`), then
    /// sends only status back over the channel.
    pub fn spawn(
        &self,
        shader_id: usize,
        source: String,
        cache_key: u64,
        cache_dir: Option<std::path::PathBuf>,
        artifact_ext: &'static str,
        compile_fn: fn(&str) -> Result<Vec<u8>, String>,
    ) -> bool {
        let tx = {
            let mut inner = self.inner.lock().unwrap();
            if !inner.pending.insert(shader_id) {
                return false;
            }
            inner.tx.clone()
        };
        std::thread::Builder::new()
            .name(format!("shader-compile-{}", shader_id))
            .spawn(move || {
                let status = match compile_fn(&source) {
                    Ok(bytes) => {
                        if let Some(dir) = &cache_dir {
                            if !bytes.is_empty() {
                                let path =
                                    dir.join(format!("{:016x}.{}", cache_key, artifact_ext));
                                let _ = std::fs::write(&path, &bytes);
                            }
                        }
                        Ok(())
                    }
                    Err(e) => Err(e),
                };
                let _ = tx.send(AsyncCompileResult { shader_id, status });
            })
            .expect("failed to spawn shader compile worker");
        true
    }

    /// Collect finished workers, then hand back at most `budget` for GPU object
    /// creation this frame; the rest stay queued. Returns `(results, has_more)`.
    pub fn drain_ready(&self, budget: usize) -> (Vec<AsyncCompileResult>, bool) {
        debug_assert!(budget >= 1, "budget must be >= 1 or the backlog never drains");
        let mut inner = self.inner.lock().unwrap();
        while let Ok(result) = inner.rx.try_recv() {
            inner.pending.remove(&result.shader_id);
            inner.ready_backlog.push_back(result);
        }
        let take = budget.min(inner.ready_backlog.len());
        let out: Vec<AsyncCompileResult> = inner.ready_backlog.drain(..take).collect();
        let has_more = !inner.ready_backlog.is_empty();
        (out, has_more)
    }
}

impl Default for AsyncShaderCompile {
    fn default() -> Self {
        Self::new()
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

    #[test]
    fn async_compile_roundtrips_via_disk_and_dedups() {
        let dir = std::env::temp_dir().join("makepad_test_async_compile");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        fn fake_compile(src: &str) -> Result<Vec<u8>, String> {
            Ok(format!("ARTIFACT:{}", src).into_bytes())
        }

        let q = AsyncShaderCompile::new();
        let key = shader_cache_key("src", 1);
        assert!(q.spawn(7, "src".into(), key, Some(dir.clone()), "bin", fake_compile));
        // duplicate spawn for same in-flight id is a no-op
        assert!(!q.spawn(7, "src".into(), key, Some(dir.clone()), "bin", fake_compile));

        // drain until the worker reports done (bounded spin, no sleep API needed)
        let mut got = None;
        for _ in 0..100_000 {
            let (ready, _more) = q.drain_ready(SHADER_CREATE_BUDGET);
            if let Some(r) = ready.into_iter().next() {
                got = Some(r);
                break;
            }
        }
        let r = got.expect("worker never reported completion");
        assert_eq!(r.shader_id, 7);
        assert!(r.status.is_ok());

        // artifact landed on disk at the expected path
        let path = dir.join(format!("{:016x}.bin", key));
        assert_eq!(std::fs::read(&path).unwrap(), b"ARTIFACT:src");
    }
}
