use std::path::Path;
use std::process::{Command, Stdio};

use crate::config::{TargetConfig, TransportKind};
use crate::error::{ExpriError, Result, command_exit_code};
use crate::shell;

#[derive(Clone, Debug)]
pub struct Remote {
  host: String,
  pub remote_dir: String,
  transport: Transport,
  port: Option<u16>,
  pub dry_run: bool,
  pub verbosity: u8,
  pub quiet: bool,
}

#[derive(Clone, Debug)]
enum Transport {
  Ssh {
    control_path: String,
    control_persist: String,
  },
  Ctl {
    bin: String,
    method: Option<String>,
  },
}

impl Remote {
  pub fn new(
    target: TargetConfig,
    control_path: String,
    control_persist: String,
    dry_run: bool,
    verbosity: u8,
    quiet: bool,
  ) -> Result<Self> {
    if target.host.is_empty() || target.host.starts_with('-') {
      return Err(ExpriError::Message(
        "target host must be nonempty and must not start with '-'".to_string(),
      ));
    }
    let (transport, host, port) = match target.transport {
      TransportKind::Ssh => {
        if target.ctl_bin.is_some() || target.ctl_method.is_some() {
          return Err(ExpriError::Message(
            "ctl_bin and ctl_method require transport = \"ctl\"".to_string(),
          ));
        }
        let (host, parsed_port) = parse_host_port(&target.host);
        (
          Transport::Ssh {
            control_path,
            control_persist,
          },
          host,
          target.port.or(parsed_port),
        )
      }
      TransportKind::Ctl => {
        if !rsync_host_compatible(&target.host) {
          return Err(ExpriError::Message(
            "ctl host cannot contain ':' or '/'; use a saved host ID or SSH alias and configure port separately".to_string(),
          ));
        }
        let bin = target.ctl_bin.unwrap_or_else(|| "ctl".to_string());
        if bin.is_empty() || target.ctl_method.as_deref() == Some("") {
          return Err(ExpriError::Message(
            "ctl_bin and ctl_method must be nonempty when configured".to_string(),
          ));
        }
        (
          Transport::Ctl {
            bin,
            method: target.ctl_method,
          },
          target.host,
          target.port,
        )
      }
    };
    Ok(Self {
      host,
      remote_dir: target.remote_dir,
      transport,
      port,
      dry_run,
      verbosity,
      quiet,
    })
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

  pub fn execute(&self, remote_command: &str) -> Result<()> {
    self.run(
      self.command_program(),
      self.command_args(&profile_command(remote_command)),
    )
  }

  pub fn execute_success(&self, remote_command: &str) -> Result<bool> {
    // Keep a remote predicate's false result separate from a transport failure.
    // The shell emits the result only after ctl/SSH establishes the connection.
    let output = self.capture_bytes(&format!(
      "if (\n{}\n) >/dev/null 2>&1; then printf 1; else printf 0; fi",
      profile_command(remote_command),
    ))?;
    if self.dry_run {
      return Ok(true);
    }
    match output.as_slice() {
      b"1" => Ok(true),
      b"0" => Ok(false),
      _ => Err(ExpriError::Message(
        "remote predicate returned unexpected output".to_string(),
      )),
    }
  }

  pub fn capture_bytes(&self, remote_command: &str) -> Result<Vec<u8>> {
    let program = self.command_program();
    let args = self.command_args(remote_command);
    if self.show_commands() && !self.quiet {
      print_command(program, &args);
    }
    if self.dry_run {
      return Ok(Vec::new());
    }
    let output = Command::new(program)
      .args(args)
      .stderr(Stdio::inherit())
      .output()
      .map_err(|source| command_launch_error(program, source))?;
    if !output.status.success() {
      return Err(ExpriError::CommandFailed {
        program: program.to_string(),
        code: command_exit_code(&output.status),
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

  pub fn connect(&self) -> Result<()> {
    let Transport::Ssh {
      control_path,
      control_persist,
    } = &self.transport
    else {
      // ctl resolves the selected host/method and owns connection reuse.
      return Ok(());
    };
    let mut check_args = vec![
      "-S".to_string(),
      control_path.clone(),
      "-O".to_string(),
      "check".to_string(),
    ];
    self.append_port(&mut check_args);
    check_args.push(self.host.clone());
    if self.status_success("ssh", self.with_verbosity(check_args), false)? {
      if self.verbosity > 0 && !self.quiet {
        eprintln!("reusing existing ssh master");
      }
      return Ok(());
    }
    let mut args = Vec::new();
    args.push("-M".to_string());
    args.push("-S".to_string());
    args.push(control_path.clone());
    args.push("-o".to_string());
    args.push(format!("ControlPersist={control_persist}"));
    args.push("-fN".to_string());
    self.append_port(&mut args);
    args.push(self.host.clone());
    self.run("ssh", self.with_verbosity(args))
  }

  fn status_success(&self, program: &str, args: Vec<String>, dry_success: bool) -> Result<bool> {
    if self.show_commands() && !self.quiet {
      print_command(program, &args);
    }
    if self.dry_run {
      return Ok(dry_success);
    }
    let status = Command::new(program)
      .args(args)
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()
      .map_err(|source| command_launch_error(program, source))?;
    Ok(status.success())
  }

  fn command_program(&self) -> &str {
    match &self.transport {
      Transport::Ssh { .. } => "ssh",
      Transport::Ctl { bin, .. } => bin,
    }
  }

  fn command_args(&self, remote_command: &str) -> Vec<String> {
    let mut args = self.remote_shell_args();
    args.push("--".to_string());
    args.push(self.host.clone());
    args.push(remote_command.to_string());
    args
  }

  fn remote_shell_args(&self) -> Vec<String> {
    let mut args = Vec::new();
    let mut ssh_args = match &self.transport {
      Transport::Ssh {
        control_path,
        control_persist,
      } => vec![
        "-S".to_string(),
        control_path.clone(),
        "-o".to_string(),
        "ControlMaster=auto".to_string(),
        "-o".to_string(),
        format!("ControlPersist={control_persist}"),
      ],
      Transport::Ctl { method, .. } => {
        if let Some(method) = method {
          args.push("--method".to_string());
          args.push(method.clone());
        }
        args.push("ssh".to_string());
        Vec::new()
      }
    };
    self.append_port(&mut ssh_args);
    args.extend(self.with_verbosity(ssh_args));
    args
  }

  fn append_port(&self, args: &mut Vec<String>) {
    if let Some(port) = self.port {
      args.push("-p".to_string());
      args.push(port.to_string());
    }
  }

  fn rsync_base_args(&self) -> Vec<String> {
    let mut args = vec![
      "-az".to_string(),
      "--no-owner".to_string(),
      "--no-group".to_string(),
      "-e".to_string(),
      join_rsync_shell(&{
        let mut args = vec![self.command_program().to_string()];
        args.extend(self.remote_shell_args());
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
    let status = Command::new(program)
      .args(args)
      .status()
      .map_err(|source| command_launch_error(program, source))?;
    if !status.success() {
      return Err(ExpriError::CommandFailed {
        program: program.to_string(),
        code: command_exit_code(&status),
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

fn profile_command(remote_command: &str) -> String {
  format!("[ -f ~/.profile ] && . ~/.profile; {remote_command}")
}

fn rsync_host_compatible(value: &str) -> bool {
  // rsync treats these delimiters as a path or remote-spec separator. Its
  // bracketed IPv6 parsing also varies by implementation; use a host ID/alias.
  !value.contains([':', '/'])
}

fn command_launch_error(program: &str, source: std::io::Error) -> ExpriError {
  ExpriError::IoContext {
    action: "launch",
    path: program.to_string(),
    source,
  }
}

// rsync parses -e itself. It escapes a quote by doubling it, not by using
// POSIX shell backslashes. This preserves paths and method names verbatim.
fn join_rsync_shell(parts: &[String]) -> String {
  parts
    .iter()
    .map(|part| format!("'{}'", part.replace('\'', "''")))
    .collect::<Vec<_>>()
    .join(" ")
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

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
