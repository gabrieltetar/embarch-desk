//! The always-on part: a systemd user service (proposal decision: the desk
//! runs beside Claude Code — in WSL on a machine like the owner's — not as a
//! Windows service beside Core).

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::Command;

const UNIT: &str = "embarch-desk.service";

fn unit_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(base.join("systemd").join("user").join(UNIT))
}

/// One word of a unit line, double-quoted so a space stays inside it: `\`
/// and `"` escaped, `%` doubled because systemd expands specifiers.
fn quoted(word: &str) -> String {
    format!("\"{}\"", word.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%"))
}

/// The unit text. PATH is carried over from the installing shell: the desk
/// runs `claude`, `gh`, `git` and `wslpath`, and a user service starts with
/// systemd's bare default PATH, which has none of the user's own bins. Under
/// WSL that PATH holds Windows folders like `/mnt/c/Program Files/…`, and
/// systemd splits an unquoted assignment at the first space, so both the
/// assignment and the binary's path are quoted; `$` is doubled in ExecStart
/// alone, the one of the two that expands variables.
pub fn unit_text(exe: &str, path_env: &str) -> String {
    let exe = quoted(&exe.replace('$', "$$"));
    let path = quoted(&format!("PATH={path_env}"));
    format!(
        "[Unit]\n\
         Description=EmbArch desk: work items, connectors and time tiles\n\
         After=network-online.target\n\n\
         [Service]\n\
         ExecStart={exe} serve\n\
         Environment={path}\n\
         Restart=on-failure\n\
         RestartSec=5\n\n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

fn systemctl(args: &[&str]) -> Result<String> {
    let out = Command::new("systemctl").arg("--user").args(args).output().context("running systemctl --user")?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if !out.status.success() && args[0] != "status" {
        bail!("systemctl --user {}: {}", args.join(" "), text.trim());
    }
    Ok(text)
}

pub fn install() -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let path_env = std::env::var("PATH").unwrap_or_default();
    let p = unit_path()?;
    std::fs::create_dir_all(p.parent().expect("unit dir"))?;
    std::fs::write(&p, unit_text(&exe.display().to_string(), &path_env))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", UNIT])?;
    systemctl(&["restart", UNIT])?;
    println!("installed {} → {} serve; it starts with your user session", p.display(), exe.display());
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let _ = systemctl(&["disable", "--now", UNIT]);
    let p = unit_path()?;
    if p.exists() {
        std::fs::remove_file(&p)?;
    }
    systemctl(&["daemon-reload"])?;
    println!("removed {}; your state and tokens are untouched", p.display());
    Ok(())
}

pub fn status() -> Result<()> {
    print!("{}", systemctl(&["status", "--no-pager", UNIT])?);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_unit_runs_serve_with_the_users_path() {
        let u = super::unit_text("/h/.local/share/embarch/bin/embarch-desk", "/h/.local/bin:/usr/bin");
        assert!(u.contains("\nExecStart=\"/h/.local/share/embarch/bin/embarch-desk\" serve\n"));
        assert!(u.contains("\nEnvironment=\"PATH=/h/.local/bin:/usr/bin\"\n"));
        assert!(u.contains("WantedBy=default.target"));
    }

    #[test]
    fn a_path_with_spaces_stays_one_assignment() {
        let u = super::unit_text(
            "/h/My Apps/$x/embarch-desk",
            "/h/.local/bin:/mnt/c/Program Files/Git/cmd:/mnt/c/Users/u/AppData/Local/Programs/Microsoft VS Code/bin:/mnt/c/50%\\\"q\"",
        );
        assert!(u.contains("\nExecStart=\"/h/My Apps/$$x/embarch-desk\" serve\n"));
        assert!(u.contains(
            "\nEnvironment=\"PATH=/h/.local/bin:/mnt/c/Program Files/Git/cmd:/mnt/c/Users/u/AppData/Local/Programs/Microsoft VS Code/bin:/mnt/c/50%%\\\\\\\"q\\\"\"\n"
        ));
    }
}
