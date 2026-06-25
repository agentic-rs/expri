use std::path::Path;
use std::process::{Command, Stdio};

use crate::config::TargetConfig;
use crate::error::{ExpriError, Result};
use crate::shell;

#[derive(Clone, Debug)]
pub struct Remote {
  pub host: String,
  pub remote_dir: String,
  pub control_path: String,
  pub control_persist: String,
  pub port: Option<u16>,
  pub dry_run: bool,
  pub verbosity: u8,
  pub quiet: bool,
}

impl Remote {
  pub fn new(
    target: TargetConfig,
    control_path: String,
    control_persist: String,
    dry_run: bool,
    verbosity: u8,
    quiet: bool,
  ) -> Self {
    let (host, parsed_port) = parse_host_port(&target.host);
    Self {
      host,
      remote_dir: target.remote_dir,
      control_path,
      control_persist,
      port: target.port.or(parsed_port),
      dry_run,
      verbosity,
      quiet,
    }
  }

  pub fn quoted_remote_dir(&self) -> String {
    shell::quote(&self.remote_dir)
  }

  pub fn meta_dir(&self) -> String {
    format!("{}/.expri", self.quoted_remote_dir())
  }

  pub fn show_commands(&self) -> bool {
    self.verbosity > 0 || self.dry_run
  }

  pub fn ssh(&self, remote_command: &str) -> Result<()> {
    self.run(
      "ssh",
      self.ssh_args(&format!(
        "[ -f ~/.profile ] && source ~/.profile; {remote_command}"
      )),
    )
  }

  pub fn ssh_success(&self, remote_command: &str) -> Result<bool> {
    let args = self.ssh_args(remote_command);
    if self.show_commands() && !self.quiet {
      print_command("ssh", &args);
    }
    if self.dry_run {
      return Ok(true);
    }
    let status = Command::new("ssh")
      .args(args)
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()?;
    Ok(status.success())
  }

  pub fn ssh_capture_bytes(&self, remote_command: &str) -> Result<Vec<u8>> {
    let args = self.ssh_args(remote_command);
    if self.show_commands() && !self.quiet {
      print_command("ssh", &args);
    }
    if self.dry_run {
      return Ok(Vec::new());
    }
    let output = Command::new("ssh").args(args).output()?;
    if !output.status.success() {
      return Err(ExpriError::CommandFailed {
        program: "ssh".to_string(),
        code: output.status.code(),
      });
    }
    Ok(output.stdout)
  }

  pub fn upload_file(&self, local_path: &Path, remote_path: &str) -> Result<()> {
    let mut args = self.rsync_base_args();
    args.push(local_path.to_string_lossy().to_string());
    args.push(format!("{}:{}", self.host, remote_path));
    self.run("rsync", args)
  }

  pub fn upload_dir(&self, local_dir: &Path, remote_dir: &str) -> Result<()> {
    let mut args = self.rsync_base_args();
    args.push(ensure_trailing_slash(&local_dir.to_string_lossy()));
    args.push(format!(
      "{}:{}",
      self.host,
      ensure_trailing_slash(remote_dir)
    ));
    self.run("rsync", args)
  }

  pub fn download_file(&self, remote_path: &str, local_path: &Path) -> Result<()> {
    let mut args = self.rsync_base_args();
    args.push(format!("{}:{}", self.host, remote_path));
    args.push(local_path.to_string_lossy().to_string());
    self.run("rsync", args)
  }

  pub fn upload_files_from(
    &self,
    local_root: &Path,
    remote_dir: &str,
    files_from: &Path,
  ) -> Result<()> {
    let mut args = self.rsync_base_args();
    args.push("--from0".to_string());
    args.push("--files-from".to_string());
    args.push(files_from.to_string_lossy().to_string());
    args.push(ensure_trailing_slash(&local_root.to_string_lossy()));
    args.push(format!(
      "{}:{}",
      self.host,
      ensure_trailing_slash(remote_dir)
    ));
    self.run("rsync", args)
  }

  pub fn download_files_from(
    &self,
    remote_dir: &str,
    local_root: &Path,
    files_from: &Path,
  ) -> Result<()> {
    let mut args = self.rsync_base_args();
    args.push("--from0".to_string());
    args.push("--files-from".to_string());
    args.push(files_from.to_string_lossy().to_string());
    args.push(format!(
      "{}:{}",
      self.host,
      ensure_trailing_slash(remote_dir)
    ));
    args.push(ensure_trailing_slash(&local_root.to_string_lossy()));
    self.run("rsync", args)
  }

  pub fn download_dir_with_excludes(
    &self,
    remote_dir: &str,
    local_dir: &Path,
    excludes: &[String],
  ) -> Result<()> {
    let mut args = self.rsync_base_args();
    for pattern in excludes {
      args.push("--exclude".to_string());
      args.push(pattern.clone());
    }
    args.push(format!(
      "{}:{}",
      self.host,
      ensure_trailing_slash(remote_dir)
    ));
    args.push(ensure_trailing_slash(&local_dir.to_string_lossy()));
    self.run("rsync", args)
  }

  pub fn open_master(&self) -> Result<bool> {
    if self.master_running()? {
      if self.verbosity > 0 && !self.quiet {
        eprintln!("reusing existing ssh master");
      }
      return Ok(false);
    }
    let mut args = Vec::new();
    args.push("-M".to_string());
    args.push("-S".to_string());
    args.push(self.control_path.clone());
    args.push("-o".to_string());
    args.push(format!("ControlPersist={}", self.control_persist));
    args.push("-fN".to_string());
    if let Some(port) = self.port {
      args.push("-p".to_string());
      args.push(port.to_string());
    }
    args.push(self.host.clone());
    self.run("ssh", self.with_verbosity(args))?;
    Ok(true)
  }

  fn master_running(&self) -> Result<bool> {
    let args = self.ssh_control_args("check");
    if self.show_commands() && !self.quiet {
      print_command("ssh", &args);
    }
    if self.dry_run {
      return Ok(false);
    }
    let status = Command::new("ssh")
      .args(args)
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()?;
    Ok(status.success())
  }

  fn ssh_control_args(&self, operation: &str) -> Vec<String> {
    let mut args = vec![
      "-S".to_string(),
      self.control_path.clone(),
      "-O".to_string(),
      operation.to_string(),
    ];
    if let Some(port) = self.port {
      args.push("-p".to_string());
      args.push(port.to_string());
    }
    args.push(self.host.clone());
    self.with_verbosity(args)
  }

  fn ssh_args(&self, remote_command: &str) -> Vec<String> {
    let mut args = self.ssh_base_args();
    args.push(self.host.clone());
    args.push(remote_command.to_string());
    args
  }

  fn ssh_base_args(&self) -> Vec<String> {
    let mut args = vec![
      "-S".to_string(),
      self.control_path.clone(),
      "-o".to_string(),
      "ControlMaster=auto".to_string(),
      "-o".to_string(),
      format!("ControlPersist={}", self.control_persist),
    ];
    if let Some(port) = self.port {
      args.push("-p".to_string());
      args.push(port.to_string());
    }
    self.with_verbosity(args)
  }

  fn rsync_base_args(&self) -> Vec<String> {
    let mut args = vec![
      "-az".to_string(),
      "--no-owner".to_string(),
      "--no-group".to_string(),
      "-e".to_string(),
      shell::join(&{
        let mut args = vec!["ssh".to_string()];
        args.extend(self.ssh_base_args());
        args
      }),
    ];
    if self.verbosity > 0 && !self.quiet {
      args.push("--progress".to_string());
    }
    args
  }

  fn run(&self, program: &str, args: Vec<String>) -> Result<()> {
    if self.show_commands() && !self.quiet {
      print_command(program, &args);
    }
    if self.dry_run {
      return Ok(());
    }
    let status = Command::new(program).args(args).status()?;
    if !status.success() {
      return Err(ExpriError::CommandFailed {
        program: program.to_string(),
        code: status.code(),
      });
    }
    Ok(())
  }

  fn with_verbosity(&self, mut args: Vec<String>) -> Vec<String> {
    if self.quiet {
      args.insert(0, "-q".to_string());
    } else if self.verbosity > 1 {
      args.insert(
        0,
        format!("-{}", "v".repeat((self.verbosity - 1).min(3) as usize)),
      );
    }
    args
  }
}

fn parse_host_port(value: &str) -> (String, Option<u16>) {
  let Some((host, port)) = value.rsplit_once(':') else {
    return (value.to_string(), None);
  };
  if host.is_empty() || host.ends_with(']') {
    return (value.to_string(), None);
  }
  match port.parse::<u16>() {
    Ok(port) => (host.to_string(), Some(port)),
    Err(_) => (value.to_string(), None),
  }
}

fn print_command(program: &str, args: &[String]) {
  let mut parts = vec![program.to_string()];
  parts.extend(args.iter().cloned());
  eprintln!("+ {}", shell::join(&parts));
}

fn ensure_trailing_slash(value: &str) -> String {
  if value.ends_with('/') {
    value.to_string()
  } else {
    format!("{value}/")
  }
}
