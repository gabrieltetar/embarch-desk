//! Where the desk keeps things. Two places, both outside every repo
//! (proposal decision: connector data never reaches an EmbArch repo):
//!
//! - **config** (`~/.config/embarch-desk/`): `connectors.toml` holding the
//!   pasted tokens, and `desk-token`, the bearer every caller of the desk's
//!   HTTP surface presents. Both owner-only, the treatment Core's token gets.
//! - **data** (`~/.local/share/embarch-desk/`): `state.json` and the
//!   monthly history files acted-on tiles move to.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub const DEFAULT_PORT: u16 = 4891;

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// `EMBARCH_DESK_CONFIG_DIR` overrides it, which is how the tests and a
/// scratch instance stay off the real files.
pub fn config_dir() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("EMBARCH_DESK_CONFIG_DIR") {
        return Ok(PathBuf::from(d));
    }
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => home()?.join(".config"),
    };
    Ok(base.join("embarch-desk"))
}

pub fn data_dir() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("EMBARCH_DESK_DATA_DIR") {
        return Ok(PathBuf::from(d));
    }
    let base = match std::env::var_os("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => home()?.join(".local").join("share"),
    };
    Ok(base.join("embarch-desk"))
}

pub fn port() -> u16 {
    std::env::var("EMBARCH_DESK_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

pub fn base_url() -> String {
    format!("http://127.0.0.1:{}", port())
}

/// Write a file only its owner can read, atomically: a temp file in the same
/// directory, then a rename, so a reader never sees half a file.
pub fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path.parent().context("path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = path.with_extension("tmp");
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// The bearer the desk checks, generated on first read. Hooks, the MCP
/// server and `embarch-ui` all run as the same user and read the same file.
pub fn desk_token() -> Result<String> {
    let path = config_dir()?.join("desk-token");
    if let Ok(t) = std::fs::read_to_string(&path) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    let token = random_hex(32)?;
    write_private(&path, token.as_bytes())?;
    Ok(token)
}

pub fn random_hex(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::getrandom(&mut buf).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_private_file_is_owner_only_and_complete() {
        let dir = std::env::temp_dir().join(format!("desk-paths-{}", random_hex(4).unwrap()));
        let p = dir.join("secret");
        write_private(&p, b"abc").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"abc");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert!(!p.with_extension("tmp").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
