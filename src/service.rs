//! The always-on part: a systemd user service (proposal decision: the desk
//! runs beside Claude Code — in WSL on a machine like the owner's — not as a
//! Windows service beside Core).
//!
//! A second desk can run beside the first as a named **instance**: the same
//! binary under `embarch-desk-NAME.service`, with its own port, config and
//! data folders, and its own Claude login and `gh` account.

use anyhow::{ensure, Context, Result};
use embarch_user_unit::{xdg_dir, Unit};

const UNIT: &str = "embarch-desk.service";

/// What makes a second desk different from the first. Every field is carried
/// into the unit as an environment variable the desk already reads.
#[derive(Debug, Clone, PartialEq)]
pub struct Instance {
    pub name: String,
    pub port: u16,
    pub config_dir: String,
    pub data_dir: String,
    pub claude_config_dir: Option<String>,
    pub gh_config_dir: Option<String>,
    pub time: bool,
    pub direct_writes: bool,
    pub suite_root: Option<String>,
    pub suite_docs: Option<String>,
}

impl Instance {
    /// `None` when no instance was named: the first desk, installed as ever.
    pub fn new(
        name: Option<String>,
        port: Option<u16>,
        claude_config_dir: Option<String>,
        gh_config_dir: Option<String>,
        no_time: bool,
        direct_writes: bool,
        (suite_root, suite_docs): (Option<String>, Option<String>),
    ) -> Result<Option<Instance>> {
        let Some(name) = name else {
            ensure!(
                port.is_none() && claude_config_dir.is_none() && gh_config_dir.is_none() && !no_time && !direct_writes && suite_root.is_none(),
                "--port, --claude-config-dir, --gh-config-dir, --no-time, --direct-writes and --suite-root need --instance NAME"
            );
            return Ok(None);
        };
        validate_name(&name)?;
        let port = port.context("an instance needs its own --port")?;
        ensure!(port != crate::paths::DEFAULT_PORT, "port {port} is the first desk's");
        Ok(Some(Instance {
            config_dir: xdg_dir("XDG_CONFIG_HOME", &[".config"])?.join(format!("embarch-desk-{name}")).display().to_string(),
            data_dir: xdg_dir("XDG_DATA_HOME", &[".local", "share"])?.join(format!("embarch-desk-{name}")).display().to_string(),
            name,
            port,
            claude_config_dir,
            gh_config_dir,
            time: !no_time,
            direct_writes,
            suite_root,
            suite_docs,
        }))
    }

    fn unit(&self) -> String {
        format!("embarch-desk-{}.service", self.name)
    }
}

/// A name becomes part of a unit file name and two folder names.
fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "an instance name is lowercase letters, digits and hyphens"
    );
    Ok(())
}

fn unit_name(instance: Option<&str>) -> Result<String> {
    match instance {
        None => Ok(UNIT.to_string()),
        Some(n) => {
            validate_name(n)?;
            Ok(format!("embarch-desk-{n}.service"))
        }
    }
}

/// The unit text. PATH is carried over from the installing shell: the desk
/// runs `claude`, `gh`, `git` and `wslpath`, and a user service starts with
/// systemd's bare default PATH, which has none of the user's own bins. Under
/// WSL that PATH holds Windows folders like `/mnt/c/Program Files/…`, and
/// systemd splits an unquoted assignment at the first space, so both the
/// assignment and the binary's path are quoted; `$` is doubled in ExecStart
/// alone, the one of the two that expands variables.
pub fn unit_text(exe: &str, path_env: &str, instance: Option<&Instance>) -> String {
    let mut vars: Vec<(&str, String)> = Vec::new();
    let mut description = "EmbArch desk: work items, connectors and time tiles".to_string();
    if let Some(i) = instance {
        description = format!("EmbArch desk ({}): work items and connectors", i.name);
        vars.push(("EMBARCH_DESK_PORT", i.port.to_string()));
        vars.push(("EMBARCH_DESK_CONFIG_DIR", i.config_dir.clone()));
        vars.push(("EMBARCH_DESK_DATA_DIR", i.data_dir.clone()));
        vars.push(("EMBARCH_DESK_TIME", if i.time { "on" } else { "off" }.to_string()));
        vars.push(("EMBARCH_DESK_DIRECT_WRITES", if i.direct_writes { "on" } else { "off" }.to_string()));
        if let Some(d) = &i.claude_config_dir {
            vars.push(("CLAUDE_CONFIG_DIR", d.clone()));
        }
        if let Some(d) = &i.gh_config_dir {
            vars.push(("GH_CONFIG_DIR", d.clone()));
        }
        if let Some(d) = &i.suite_root {
            vars.push(("EMBARCH_DESK_SUITE_ROOT", d.clone()));
        }
        if let Some(d) = &i.suite_docs {
            vars.push(("EMBARCH_DESK_SUITE_DOCS", d.clone()));
        }
    }
    embarch_user_unit::unit_text(&Unit { description: &description, exe, args: &["serve"], path_env, vars: &vars })
}

pub fn install(instance: Option<Instance>) -> Result<()> {
    let unit = instance.as_ref().map_or_else(|| UNIT.to_string(), Instance::unit);
    let exe = std::env::current_exe()?.canonicalize()?;
    let path_env = std::env::var("PATH").unwrap_or_default();
    let text = unit_text(&exe.display().to_string(), &path_env, instance.as_ref());
    let p = embarch_user_unit::install(&unit, &text)?;
    println!("installed {} → {} serve; it starts with your user session", p.display(), exe.display());
    Ok(())
}

pub fn uninstall(instance: Option<&str>) -> Result<()> {
    let p = embarch_user_unit::uninstall(&unit_name(instance)?)?;
    println!("removed {}; your state and tokens are untouched", p.display());
    Ok(())
}

pub fn status(instance: Option<&str>) -> Result<()> {
    print!("{}", embarch_user_unit::status(&unit_name(instance)?)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_unit_runs_serve_with_the_users_path() {
        let u = super::unit_text("/h/.local/share/embarch/bin/embarch-desk", "/h/.local/bin:/usr/bin", None);
        assert!(u.contains("\nExecStart=\"/h/.local/share/embarch/bin/embarch-desk\" serve\n"));
        assert!(u.contains("\nEnvironment=\"PATH=/h/.local/bin:/usr/bin\"\n"));
        assert!(u.contains("WantedBy=default.target"));
    }

    /// The text the desk wrote before the writer moved to `embarch-user-unit`,
    /// pinned byte for byte, for the first desk and for an instance.
    #[test]
    fn the_unit_text_is_what_the_desk_always_wrote() {
        let first = "[Unit]\nDescription=EmbArch desk: work items, connectors and time tiles\nAfter=network-online.target\n\n[Service]\nExecStart=\"/h/My Apps/$$x/embarch-desk\" serve\nEnvironment=\"PATH=/usr/bin:/mnt/c/Program Files/Git\"\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n";
        assert_eq!(super::unit_text("/h/My Apps/$x/embarch-desk", "/usr/bin:/mnt/c/Program Files/Git", None), first);
        let i = super::Instance {
            name: "dev".into(),
            port: 4892,
            config_dir: "/c".into(),
            data_dir: "/d".into(),
            claude_config_dir: Some("/cl".into()),
            gh_config_dir: None,
            time: true,
            direct_writes: false,
            suite_root: None,
            suite_docs: Some("docs".into()),
        };
        let dev = "[Unit]\nDescription=EmbArch desk (dev): work items and connectors\nAfter=network-online.target\n\n[Service]\nExecStart=\"/b/embarch-desk\" serve\nEnvironment=\"PATH=/usr/bin\"\nEnvironment=\"EMBARCH_DESK_PORT=4892\"\nEnvironment=\"EMBARCH_DESK_CONFIG_DIR=/c\"\nEnvironment=\"EMBARCH_DESK_DATA_DIR=/d\"\nEnvironment=\"EMBARCH_DESK_TIME=on\"\nEnvironment=\"EMBARCH_DESK_DIRECT_WRITES=off\"\nEnvironment=\"CLAUDE_CONFIG_DIR=/cl\"\nEnvironment=\"EMBARCH_DESK_SUITE_DOCS=docs\"\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n";
        assert_eq!(super::unit_text("/b/embarch-desk", "/usr/bin", Some(&i)), dev);
    }

    #[test]
    fn an_instance_unit_carries_its_own_ports_folders_and_logins() {
        let i = super::Instance {
            name: "dev".into(),
            port: 4892,
            config_dir: "/h/.config/embarch-desk-dev".into(),
            data_dir: "/h/.local/share/embarch-desk-dev".into(),
            claude_config_dir: Some("/h/.claude-personal".into()),
            gh_config_dir: Some("/h/.config/gh-personal".into()),
            time: false,
            direct_writes: true,
            suite_root: Some("/h/suite".into()),
            suite_docs: Some("docs".into()),
        };
        let u = super::unit_text("/h/bin/embarch-desk", "/usr/bin", Some(&i));
        for want in [
            "Environment=\"EMBARCH_DESK_PORT=4892\"\n",
            "Environment=\"EMBARCH_DESK_CONFIG_DIR=/h/.config/embarch-desk-dev\"\n",
            "Environment=\"EMBARCH_DESK_DATA_DIR=/h/.local/share/embarch-desk-dev\"\n",
            "Environment=\"EMBARCH_DESK_TIME=off\"\n",
            "Environment=\"EMBARCH_DESK_DIRECT_WRITES=on\"\n",
            "Environment=\"CLAUDE_CONFIG_DIR=/h/.claude-personal\"\n",
            "Environment=\"GH_CONFIG_DIR=/h/.config/gh-personal\"\n",
            "Environment=\"EMBARCH_DESK_SUITE_ROOT=/h/suite\"\n",
            "Environment=\"EMBARCH_DESK_SUITE_DOCS=docs\"\n",
        ] {
            assert!(u.contains(want), "missing {want}");
        }
        assert!(!super::unit_text("/h/bin/embarch-desk", "/usr/bin", None).contains("EMBARCH_DESK_PORT"));
    }

    #[test]
    fn an_instance_needs_a_sane_name_and_a_port_of_its_own() {
        let new = |n: &str, p: Option<u16>| super::Instance::new(Some(n.into()), p, None, None, false, false, (None, None));
        assert!(new("dev", Some(4892)).is_ok());
        assert!(new("Dev/..", Some(4892)).is_err());
        assert!(new("dev", None).is_err());
        assert!(new("dev", Some(4891)).is_err());
        assert!(super::Instance::new(None, Some(4892), None, None, false, false, (None, None)).is_err());
        assert!(super::Instance::new(None, None, None, None, false, false, (None, None)).unwrap().is_none());
        assert!(super::Instance::new(None, None, None, None, false, false, (Some("/s".into()), None)).is_err());
    }
}
