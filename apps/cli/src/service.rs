//! Running clipx in the background: systemd, launchd, or a detached process as a fallback.

use std::path::{Path, PathBuf};
use std::process::Command;

pub enum Kind {
    SystemdSystem,
    SystemdUser,
    Launchd,
    Detached,
}

fn is_root() -> bool {
    Command::new("id").arg("-u").output().map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0").unwrap_or(false)
}

fn systemd_running() -> bool {
    Path::new("/run/systemd/system").exists()
}

fn user_systemd_works() -> bool {
    Command::new("systemctl").args(["--user", "is-system-running"]).output().map(|o| !o.stdout.is_empty()).unwrap_or(false)
}

fn system_unit() -> PathBuf {
    PathBuf::from("/etc/systemd/system/clipx.service")
}
fn user_unit() -> PathBuf {
    crate::util::home_dir().join(".config/systemd/user/clipx.service")
}
fn plist() -> PathBuf {
    crate::util::home_dir().join("Library/LaunchAgents/dev.clipx.plist")
}

/// The service manager that currently owns clipx, if any.
pub fn installed() -> Kind {
    if system_unit().exists() {
        Kind::SystemdSystem
    } else if user_unit().exists() {
        Kind::SystemdUser
    } else if plist().exists() {
        Kind::Launchd
    } else {
        Kind::Detached
    }
}

fn best_kind() -> Kind {
    if cfg!(target_os = "macos") {
        Kind::Launchd
    } else if systemd_running() && is_root() {
        Kind::SystemdSystem
    } else if systemd_running() && user_systemd_works() {
        Kind::SystemdUser
    } else {
        Kind::Detached
    }
}

fn run(cmd: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new(cmd).args(args).output().map_err(|e| format!("{cmd}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("{cmd} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// Install (or refresh) the background service and start it. Returns a description.
pub fn install_and_start(home: &Path) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = exe.display().to_string();
    let home_s = home.display().to_string();
    match best_kind() {
        Kind::SystemdSystem => {
            let unit = format!(
                "[Unit]\nDescription=clipx AI subscription proxy\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nExecStart={exe} serve\nEnvironment=CLIPX_HOME={home_s}\nRestart=always\nRestartSec=2\nLimitNOFILE=65536\n\n[Install]\nWantedBy=multi-user.target\n"
            );
            std::fs::write(system_unit(), unit).map_err(|e| e.to_string())?;
            run("systemctl", &["daemon-reload"])?;
            run("systemctl", &["enable", "clipx"])?;
            run("systemctl", &["restart", "clipx"])?;
            Ok("systemd service clipx (starts on boot)".into())
        }
        Kind::SystemdUser => {
            let unit = format!(
                "[Unit]\nDescription=clipx AI subscription proxy\nAfter=network-online.target\n\n[Service]\nExecStart={exe} serve\nEnvironment=CLIPX_HOME={home_s}\nRestart=always\nRestartSec=2\n\n[Install]\nWantedBy=default.target\n"
            );
            let path = user_unit();
            std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
            std::fs::write(&path, unit).map_err(|e| e.to_string())?;
            run("systemctl", &["--user", "daemon-reload"])?;
            run("systemctl", &["--user", "enable", "clipx"])?;
            run("systemctl", &["--user", "restart", "clipx"])?;
            let user = std::env::var("USER").unwrap_or_default();
            let linger = !user.is_empty() && run("loginctl", &["enable-linger", &user]).is_ok();
            Ok(if linger { "systemd user service clipx (starts on boot)".into() } else { "systemd user service clipx (starts when you log in)".into() })
        }
        Kind::Launchd => {
            let log = home.join("clipx.log").display().to_string();
            let p = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>dev.clipx</string>\n<key>ProgramArguments</key><array><string>{exe}</string><string>serve</string></array>\n<key>EnvironmentVariables</key><dict><key>CLIPX_HOME</key><string>{home_s}</string></dict>\n<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><true/>\n<key>StandardOutPath</key><string>{log}</string>\n<key>StandardErrorPath</key><string>{log}</string>\n</dict></plist>\n"
            );
            let path = plist();
            std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
            let _ = run("launchctl", &["unload", &path.display().to_string()]);
            std::fs::write(&path, p).map_err(|e| e.to_string())?;
            run("launchctl", &["load", "-w", &path.display().to_string()])?;
            Ok("launchd agent dev.clipx (starts on login)".into())
        }
        Kind::Detached => {
            stop(home);
            start_detached(home)?;
            Ok("background process (no systemd here, so it will not start on reboot; run `clipx start` after a reboot)".into())
        }
    }
}

fn pid_file(home: &Path) -> PathBuf {
    home.join("clipx.pid")
}

fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists() || (cfg!(target_os = "macos") && run("kill", &["-0", &pid.to_string()]).is_ok())
}

pub fn detached_pid(home: &Path) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(pid_file(home)).ok()?.trim().parse().ok()?;
    pid_alive(pid).then_some(pid)
}

pub fn start_detached(home: &Path) -> Result<(), String> {
    if detached_pid(home).is_some() {
        return Ok(());
    }
    let log_path = home.join("clipx.log");
    if std::fs::metadata(&log_path).map(|m| m.len() > 10 << 20).unwrap_or(false) {
        let _ = std::fs::rename(&log_path, home.join("clipx.log.1"));
    }
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path).map_err(|e| e.to_string())?;
    let mut cmd = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
    cmd.arg("serve")
        .env("CLIPX_HOME", home)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    std::fs::write(pid_file(home), child.id().to_string()).map_err(|e| e.to_string())?;
    std::thread::sleep(std::time::Duration::from_millis(400));
    if let Ok(Some(status)) = child.try_wait() {
        let _ = std::fs::remove_file(pid_file(home));
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        let tail: Vec<&str> = log.lines().rev().take(3).collect();
        return Err(format!("clipx exited right away ({status}): {}", tail.into_iter().rev().collect::<Vec<_>>().join(" | ")));
    }
    Ok(())
}

pub fn start(home: &Path) -> Result<(), String> {
    match installed() {
        Kind::SystemdSystem => run("systemctl", &["start", "clipx"]),
        Kind::SystemdUser => run("systemctl", &["--user", "start", "clipx"]),
        Kind::Launchd => run("launchctl", &["load", "-w", &plist().display().to_string()]),
        Kind::Detached => start_detached(home),
    }
}

pub fn stop(home: &Path) {
    let _ = match installed() {
        Kind::SystemdSystem => run("systemctl", &["stop", "clipx"]),
        Kind::SystemdUser => run("systemctl", &["--user", "stop", "clipx"]),
        Kind::Launchd => run("launchctl", &["unload", &plist().display().to_string()]),
        Kind::Detached => Ok(()),
    };
    if let Some(pid) = detached_pid(home) {
        let _ = run("kill", &[&pid.to_string()]);
        for _ in 0..50 {
            if !pid_alive(pid) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = std::fs::remove_file(pid_file(home));
    }
}

pub fn restart(home: &Path) -> Result<(), String> {
    match installed() {
        Kind::SystemdSystem => run("systemctl", &["restart", "clipx"]),
        Kind::SystemdUser => run("systemctl", &["--user", "restart", "clipx"]),
        _ => {
            stop(home);
            start(home)
        }
    }
}

pub fn uninstall(home: &Path) {
    stop(home);
    match installed() {
        Kind::SystemdSystem => {
            let _ = run("systemctl", &["disable", "clipx"]);
            let _ = std::fs::remove_file(system_unit());
            let _ = run("systemctl", &["daemon-reload"]);
        }
        Kind::SystemdUser => {
            let _ = run("systemctl", &["--user", "disable", "clipx"]);
            let _ = std::fs::remove_file(user_unit());
            let _ = run("systemctl", &["--user", "daemon-reload"]);
        }
        Kind::Launchd => {
            let _ = std::fs::remove_file(plist());
        }
        Kind::Detached => {}
    }
}

pub fn logs(home: &Path, follow: bool) {
    let status = match installed() {
        Kind::SystemdSystem => Command::new("journalctl").args(["-u", "clipx", "-n", "200"]).args(follow.then_some("-f")).status(),
        Kind::SystemdUser => Command::new("journalctl").args(["--user", "-u", "clipx", "-n", "200"]).args(follow.then_some("-f")).status(),
        _ => Command::new("tail").args(["-n", "200"]).args(follow.then_some("-f")).arg(home.join("clipx.log")).status(),
    };
    if let Err(e) = status {
        eprintln!("could not read logs: {e}");
    }
}
