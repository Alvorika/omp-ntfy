use anyhow::Context;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const UNIT_NAME: &str = "omp-ntfy.service";

pub fn unit_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| {
        format!("{}/.config", std::env::var("HOME").unwrap_or_default())
    });
    PathBuf::from(base).join("systemd/user").join(UNIT_NAME)
}

pub fn unit_contents(bin: &Path, config_dir: &Path, data_dir: &Path) -> String {
    format!(
        "# Managed by `omp-ntfy service install`. Remove with `omp-ntfy service uninstall`.\n\
         [Unit]\n\
         Description=omp-ntfy Web Push notification server\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={bin} serve --config-dir {config} --data-dir {data}\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         NoNewPrivileges=true\n\
         ProtectSystem=strict\n\
         ProtectHome=read-only\n\
         ReadWritePaths={data}\n\
         PrivateTmp=true\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        bin = bin.display(),
        config = config_dir.display(),
        data = data_dir.display(),
    )
}

pub fn systemctl(args: &[&str]) -> anyhow::Result<std::process::ExitStatus> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .context("failed to run systemctl (is systemd user session available?)")?;
    if !out.stdout.is_empty() {
        print!("{}", String::from_utf8_lossy(&out.stdout));
    }
    if !out.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(out.status)
}

pub fn install(bin: &Path, config_dir: &Path, data_dir: &Path) -> anyhow::Result<()> {
    let path = unit_path();
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&path, unit_contents(bin, config_dir, data_dir))?;
    println!("wrote {}", path.display());
    systemctl(&["daemon-reload"])?;
    let st = systemctl(&["enable", "--now", UNIT_NAME])?;
    anyhow::ensure!(st.success(), "systemctl enable --now failed");
    println!("enabled and started {UNIT_NAME}");
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    systemctl(&["disable", "--now", UNIT_NAME])?;
    let path = unit_path();
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    systemctl(&["daemon-reload"])?;
    println!("disabled and removed {UNIT_NAME}");
    Ok(())
}