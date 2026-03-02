use pica_pkg_core::error::{PicaError, PicaResult};
use pica_pkg_core::io::sha256_file;
use std::fs;
use std::path::Path;
use std::process::Command;

pub(crate) fn run_tar_create(base_dir: &Path, output: &Path, items: &[String]) -> PicaResult<()> {
  let mut command = Command::new("tar");
  command.arg("-C");
  command.arg(base_dir);
  command.arg("-czf");
  command.arg(output);
  for item in items {
    command.arg(item);
  }

  let result = command.output()?;
  if result.status.success() {
    Ok(())
  } else {
    let stderr = String::from_utf8_lossy(&result.stderr).trim().to_string();
    Err(PicaError::msg(format!(
      "tar create failed: {}",
      if stderr.is_empty() { "unknown error" } else { &stderr }
    )))
  }
}

pub(crate) fn write_sha256sum_entry(path: &Path, filename: &str, sha256: &str) -> PicaResult<()> {
  let mut lines: Vec<String> = if path.is_file() {
    fs::read_to_string(path)?.lines().map(ToString::to_string).collect()
  } else {
    Vec::new()
  };

  lines.retain(|line| {
    let trimmed = line.trim_end();
    !trimmed.ends_with(&format!("  {filename}"))
  });
  lines.push(format!("{sha256}  {filename}"));
  lines.sort();

  let mut content = lines.join("\n");
  content.push('\n');
  fs::write(path, content)?;
  Ok(())
}

pub(crate) fn compute_sha256(path: &Path) -> PicaResult<String> {
  sha256_file(path)
}
