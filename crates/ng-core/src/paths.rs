//! Filesystem locations for not-goldfish state.

use std::path::PathBuf;

/// Root data dir: `$NG_DATA_DIR` or `~/.not-goldfish`.
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("NG_DATA_DIR") {
        return PathBuf::from(dir);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".not-goldfish")
}

pub fn db_path() -> PathBuf {
    data_dir().join("ng.db")
}

/// Unix sockets are limited to ~108 bytes of path (SUN_LEN). Prefer the
/// data dir, but fall back to the runtime dir / tmp when it is too deep.
pub fn socket_path() -> PathBuf {
    let preferred = data_dir().join("ngd.sock");
    if preferred.as_os_str().len() < 90 {
        return preferred;
    }
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(runtime).join(format!("ngd-{}.sock", hash_path(&data_dir())))
}

/// Stable short hash so distinct NG_DATA_DIRs get distinct sockets.
fn hash_path(path: &std::path::Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

pub fn pid_path() -> PathBuf {
    data_dir().join("ngd.pid")
}
