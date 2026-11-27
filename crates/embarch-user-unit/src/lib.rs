//! A systemd user unit for an EmbArch binary, written once for the suite: the
//! desk and the UI both install themselves through it. It renders the unit's
//! text, knows where a user unit lives, and wraps `systemctl --user`. It knows
//! nothing about what the binary is, or about instances.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// What one unit says. `args` follow the executable on `ExecStart`.
pub struct Unit<'a> {
    pub description: &'a str,
    pub exe: &'a str,
    pub args: &'a [&'a str],
    /// The installing shell's `PATH`, carried because a user service starts with
    /// systemd's bare default.
    pub path_env: &'a str,
    /// `Environment=` pairs written after `PATH`, in order.
    pub vars: &'a [(&'a str, String)],
}

/// One word of a unit line, double-quoted so a space stays inside it: `\`
/// and `"` escaped, `%` doubled because systemd expands specifiers.
pub fn quoted(word: &str) -> String {
    format!("\"{}\"", word.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%"))
}

/// The unit text. Under WSL `PATH` holds Windows folders like
/// `/mnt/c/Program Files/…`, and systemd splits an unquoted assignment at the
/// first space, so every assignment and the binary's path are quoted; `$` is
/// doubled in `ExecStart` alone, the one of the two that expands variables.
pub fn unit_text(u: &Unit) -> String {
    let exec: String = std::iter::once(u.exe.replace('$', "$$"))
        .chain(u.args.iter().map(|a| a.replace('$', "$$")))
        .enumerate()
        .map(|(i, w)| if i == 0 { quoted(&w) } else { format!(" {w}") })
        .collect();
    let env: String = std::iter::once(format!("PATH={}", u.path_env))
        .chain(u.vars.iter().map(|(k, v)| format!("{k}={v}")))
        .map(|e| format!("Environment={}\n", quoted(&e)))
        .collect();
    format!(
        "[Unit]\n\
         Description={}\n\
         After=network-online.target\n\n\
         [Service]\n\
         ExecStart={exec}\n\
         {env}\
         Restart=on-failure\n\
         RestartSec=5\n\n\
         [Install]\n\
         WantedBy=default.target\n",
        u.description
    )
}

/// `$var` when set and non-empty, else `$HOME` joined with `under_home`.
pub fn xdg_dir(var: &str, under_home: &[&str]) -> Result<PathBuf> {
    match std::env::var_os(var).filter(|s| !s.is_empty()) {
        Some(x) => Ok(PathBuf::from(x)),
        None => {
            let mut p = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
            p.extend(under_home);
            Ok(p)
        }
    }
}

/// Where a user unit lives: under `$XDG_CONFIG_HOME` (empty read as unset) or
/// `~/.config`, in `systemd/user`.
pub fn unit_path(unit: &str) -> Result<PathBuf> {
    Ok(xdg_dir("XDG_CONFIG_HOME", &[".config"])?.join("systemd").join("user").join(unit))
}

/// Runs `systemctl --user ARGS` and returns stdout and stderr together. A
/// failure is an error carrying that text, except for `status`, whose exit code
/// says only that the unit is not running.
pub fn systemctl(args: &[&str]) -> Result<String> {
    let out = Command::new("systemctl").arg("--user").args(args).output().context("running systemctl --user")?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if !out.status.success() && args[0] != "status" {
        bail!("systemctl --user {}: {}", args.join(" "), text.trim());
    }
    Ok(text)
}

/// Writes the unit file, reloads systemd, enables the unit and (re)starts it.
/// Returns the file's path.
pub fn install(name: &str, text: &str) -> Result<PathBuf> {
    let p = unit_path(name)?;
    std::fs::create_dir_all(p.parent().expect("unit dir"))?;
    std::fs::write(&p, text)?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", name])?;
    systemctl(&["restart", name])?;
    Ok(p)
}

/// Disables the unit (a failure there is ignored: it may never have been
/// installed), removes its file and reloads systemd. Returns the file's path.
pub fn uninstall(name: &str) -> Result<PathBuf> {
    let _ = systemctl(&["disable", "--now", name]);
    let p = unit_path(name)?;
    remove_if_present(&p)?;
    systemctl(&["daemon-reload"])?;
    Ok(p)
}

fn remove_if_present(p: &Path) -> Result<()> {
    if p.exists() {
        std::fs::remove_file(p)?;
    }
    Ok(())
}

/// `systemctl --user status --no-pager NAME`, whatever its exit code.
pub fn status(name: &str) -> Result<String> {
    systemctl(&["status", "--no-pager", name])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit<'a>(exe: &'a str, path: &'a str, args: &'a [&'a str], vars: &'a [(&'a str, String)]) -> String {
        unit_text(&Unit { description: "d", exe, args, path_env: path, vars })
    }

    #[test]
    fn the_unit_runs_the_binary_with_the_users_path() {
        let u = unit("/h/bin/x", "/h/.local/bin:/usr/bin", &["serve"], &[]);
        assert!(u.contains("\nExecStart=\"/h/bin/x\" serve\n"), "{u}");
        assert!(u.contains("\nEnvironment=\"PATH=/h/.local/bin:/usr/bin\"\n"), "{u}");
        assert!(u.contains("\nDescription=d\n"));
        assert!(u.contains("\nRestart=on-failure\n"));
        assert!(u.contains("WantedBy=default.target"));
        assert!(unit("/h/bin/x", "/usr/bin", &[], &[]).contains("\nExecStart=\"/h/bin/x\"\n"));
    }

    #[test]
    fn a_path_with_spaces_stays_one_assignment() {
        let u = unit(
            "/h/My Apps/$x/embarch-desk",
            "/h/.local/bin:/mnt/c/Program Files/Git/cmd:/mnt/c/Users/u/AppData/Local/Programs/Microsoft VS Code/bin:/mnt/c/50%\\\"q\"",
            &["serve"],
            &[],
        );
        assert!(u.contains("\nExecStart=\"/h/My Apps/$$x/embarch-desk\" serve\n"));
        assert!(u.contains(
            "\nEnvironment=\"PATH=/h/.local/bin:/mnt/c/Program Files/Git/cmd:/mnt/c/Users/u/AppData/Local/Programs/Microsoft VS Code/bin:/mnt/c/50%%\\\\\\\"q\\\"\"\n"
        ));
    }

    #[test]
    fn the_variables_follow_path_in_order_and_are_quoted() {
        let vars = [("A", "1".to_string()), ("B", "two words".to_string())];
        let u = unit("/h/bin/x", "/usr/bin", &[], &vars);
        assert!(u.contains("Environment=\"PATH=/usr/bin\"\nEnvironment=\"A=1\"\nEnvironment=\"B=two words\"\n"), "{u}");
    }

    #[test]
    fn a_user_unit_lives_under_xdg_config_home_or_home_dot_config() {
        // One test mutates the environment, so it cannot race another.
        let (xdg, home) = (std::env::var_os("XDG_CONFIG_HOME"), std::env::var_os("HOME"));
        std::env::set_var("HOME", "/h");
        std::env::set_var("XDG_CONFIG_HOME", "/x");
        assert_eq!(unit_path("u.service").unwrap(), PathBuf::from("/x/systemd/user/u.service"));
        std::env::set_var("XDG_CONFIG_HOME", "");
        assert_eq!(unit_path("u.service").unwrap(), PathBuf::from("/h/.config/systemd/user/u.service"));
        std::env::remove_var("XDG_CONFIG_HOME");
        assert_eq!(unit_path("u.service").unwrap(), PathBuf::from("/h/.config/systemd/user/u.service"));
        for (k, v) in [("XDG_CONFIG_HOME", xdg), ("HOME", home)] {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}
