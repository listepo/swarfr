//! The service manager's unit for `swarfr daemon run`: a launchd agent on macOS, a systemd user
//! unit on Linux. Low CPU and I/O priority are set here, not in code. A Windows service waits for
//! the Windows work (T21).

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

/// The launchd label, and the agent's file name.
pub const LABEL: &str = "dev.swarfr.daemon";
/// The systemd user unit.
pub const SYSTEMD_UNIT: &str = "swarfr.service";
/// launchd waits this long before it starts an agent that exited again, and so does systemd.
const RESTART_SECS: u32 = 300;
const UNSUPPORTED: &str = "no service manager support on this platform yet: run \
                           `swarfr daemon run` from a scheduler of your own";

/// A unit file and where it goes.
pub struct Unit {
    pub path: PathBuf,
    pub text: String,
}

/// This binary, `daemon run`, and the files the install named, made absolute: a service starts
/// in `/`, not where `install` was run.
fn program(config: Option<&Path>, index: Option<&Path>) -> Result<Vec<String>> {
    let exe = std::env::current_exe().context("finding this binary")?;
    let mut args = vec![
        exe.to_string_lossy().into_owned(),
        "daemon".into(),
        "run".into(),
    ];
    for (flag, path) in [("--config", config), ("--index", index)] {
        if let Some(path) = path {
            let path = std::path::absolute(path)
                .with_context(|| format!("resolving {}", path.display()))?;
            args.extend([flag.to_owned(), path.to_string_lossy().into_owned()]);
        }
    }
    Ok(args)
}

fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("HOME").context("HOME is not set")?,
    ))
}

/// Where this platform's unit lives, if it has one.
fn unit_path() -> Result<PathBuf> {
    let home = home()?;
    if cfg!(target_os = "macos") {
        Ok(home.join(format!("Library/LaunchAgents/{LABEL}.plist")))
    } else if cfg!(target_os = "linux") {
        let base = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(xdg) if !xdg.is_empty() => PathBuf::from(xdg),
            _ => home.join(".config"),
        };
        Ok(base.join("systemd/user").join(SYSTEMD_UNIT))
    } else {
        bail!(UNSUPPORTED)
    }
}

fn unit(program: &[String]) -> Result<Unit> {
    let path = unit_path()?;
    let text = if cfg!(target_os = "macos") {
        launchd_plist(program, &home()?.join("Library/Logs/swarfr.log"))
    } else {
        systemd_unit(program)
    };
    Ok(Unit { path, text })
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A user agent: started at login, started again when it exits, in the background band.
pub fn launchd_plist(program: &[String], log: &Path) -> String {
    let args: String = program
        .iter()
        .map(|arg| format!("    <string>{}</string>\n", xml(arg)))
        .collect();
    let log = xml(&log.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{args}  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>{RESTART_SECS}</integer>
  <key>ProcessType</key><string>Background</string>
  <key>Nice</key><integer>10</integer>
  <key>LowPriorityIO</key><true/>
  <key>LowPriorityBackgroundIO</key><true/>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#
    )
}

/// One `ExecStart` word: quoted, with systemd's specifiers and variables kept literal.
fn systemd_word(arg: &str) -> String {
    let escaped = arg
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('$', "$$");
    format!("\"{escaped}\"")
}

/// A user unit: started with the user's session, idle CPU and I/O class.
pub fn systemd_unit(program: &[String]) -> String {
    let exec: Vec<String> = program.iter().map(|arg| systemd_word(arg)).collect();
    format!(
        "[Unit]\n\
         Description=swarfr: shrink build dirs once they have gone cold\n\
         \n\
         [Service]\n\
         ExecStart={}\n\
         Restart=on-failure\n\
         RestartSec={RESTART_SECS}\n\
         Nice=19\n\
         CPUSchedulingPolicy=idle\n\
         IOSchedulingClass=idle\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exec.join(" ")
    )
}

/// Runs `program args`; it must succeed.
fn must(program: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    ensure!(
        out.status.success(),
        "{program} {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

/// Runs `program args` for its effect only: stopping what is not running is not an error.
fn tolerate(program: &str, args: &[&str]) {
    let _ = Command::new(program).args(args).output();
}

/// `gui/<uid>`, the launchd domain of the user's login session.
fn gui_domain() -> Result<String> {
    let out = Command::new("id")
        .arg("-u")
        .output()
        .context("running id")?;
    ensure!(out.status.success(), "id -u failed");
    Ok(format!(
        "gui/{}",
        String::from_utf8_lossy(&out.stdout).trim()
    ))
}

/// `swarfr daemon install`: writes the unit and has the service manager start it. `print`
/// only shows it.
pub fn install(config: Option<&Path>, index: Option<&Path>, print: bool) -> Result<()> {
    let unit = unit(&program(config, index)?)?;
    if print {
        eprintln!("{}:", unit.path.display());
        print!("{}", unit.text);
        return Ok(());
    }
    let dir = unit.path.parent().context("the unit has no parent dir")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::write(&unit.path, &unit.text)
        .with_context(|| format!("writing {}", unit.path.display()))?;
    let path = unit.path.to_string_lossy();
    if cfg!(target_os = "macos") {
        let domain = gui_domain()?;
        // Loaded already, from an earlier install: this one replaces it.
        tolerate("launchctl", &["bootout", &format!("{domain}/{LABEL}")]);
        must("launchctl", &["bootstrap", &domain, &path])?;
    } else {
        must("systemctl", &["--user", "daemon-reload"])?;
        must("systemctl", &["--user", "enable", "--now", SYSTEMD_UNIT])?;
    }
    println!("installed and started {}", unit.path.display());
    Ok(())
}

/// `swarfr daemon remove`: stops the daemon and removes the unit.
pub fn remove() -> Result<()> {
    let path = unit_path()?;
    if cfg!(target_os = "macos") {
        tolerate(
            "launchctl",
            &["bootout", &format!("{}/{LABEL}", gui_domain()?)],
        );
    } else {
        tolerate("systemctl", &["--user", "disable", "--now", SYSTEMD_UNIT]);
    }
    match fs::remove_file(&path) {
        Ok(()) => println!("stopped and removed {}", path.display()),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            println!("not installed: no {}", path.display());
        }
        Err(error) => return Err(error).with_context(|| format!("removing {}", path.display())),
    }
    if cfg!(target_os = "linux") {
        tolerate("systemctl", &["--user", "daemon-reload"]);
    }
    Ok(())
}

/// One line on whether the unit is there; nothing where the platform has none.
pub fn print_installed() {
    let Ok(path) = unit_path() else { return };
    if path.exists() {
        println!("installed: {}", path.display());
    } else {
        println!(
            "not installed: `swarfr daemon install` writes {}",
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program() -> Vec<String> {
        [
            "/opt/a & b/swarfr",
            "daemon",
            "run",
            "--config",
            "/c/50%$HOME\"x\".toml",
        ]
        .map(String::from)
        .to_vec()
    }

    #[test]
    fn the_plist_escapes_every_argument() {
        let plist = launchd_plist(&program(), Path::new("/l/swarfr.log"));
        assert!(
            plist.contains("<string>/opt/a &amp; b/swarfr</string>"),
            "{plist}"
        );
        assert!(plist.contains("<string>/c/50%$HOME&quot;x&quot;.toml</string>"));
        assert!(plist.contains("<key>LowPriorityIO</key><true/>"));
        assert!(plist.contains("<string>/l/swarfr.log</string>"));
    }

    #[test]
    fn the_systemd_unit_keeps_specifiers_and_variables_literal() {
        let unit = systemd_unit(&program());
        assert!(
            unit.contains(
                r#"ExecStart="/opt/a & b/swarfr" "daemon" "run" "--config" "/c/50%%$$HOME\"x\".toml""#
            ),
            "{unit}"
        );
        assert!(unit.contains("IOSchedulingClass=idle"));
    }
}
