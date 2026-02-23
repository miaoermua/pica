use crate::error::{PicaError, PicaResult};
use crate::version::pkgver_cmp_key;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoJson {
    pub schema: i64,
    pub packages: Vec<RepoPackage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoPackage {
    pub pkgname: String,
    pub pkgver: String,
    pub pkgrel: String,
    pub platform: String,
    pub arch: String,
    pub filename: String,
    #[serde(default)]
    pub appname: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub pica: Option<String>,
    #[serde(default)]
    pub download_url: Option<String>,
    #[serde(default)]
    pub manifest: Option<Value>,
}

impl RepoPackage {
    pub fn app_key(&self) -> &str {
        self.appname.as_deref().unwrap_or(&self.pkgname)
    }

    pub fn version_key(&self) -> String {
        pkgver_cmp_key(&self.pkgver, &self.pkgrel)
    }
}

pub fn parse_repo_json(content: &str) -> PicaResult<RepoJson> {
    let parsed: RepoJson = serde_json::from_str(content)?;
    validate_repo(&parsed)?;
    Ok(parsed)
}

pub fn validate_repo(repo: &RepoJson) -> PicaResult<()> {
    if repo.schema != 1 {
        return Err(PicaError::msg("schema must be 1"));
    }

    for pkg in &repo.packages {
        if pkg.pkgname.is_empty()
            || pkg.pkgver.is_empty()
            || pkg.pkgrel.is_empty()
            || pkg.platform.is_empty()
            || pkg.arch.is_empty()
            || pkg.filename.is_empty()
        {
            return Err(PicaError::msg(
                "package entry missing required string fields: pkgname/pkgver/pkgrel/platform/arch/filename",
            ));
        }

        validate_filename(pkg)?;

        if let Some(download_url) = &pkg.download_url {
            if !is_supported_url(download_url) {
                return Err(PicaError::msg(format!(
                    "package {}: invalid download_url {}",
                    pkg.pkgname, download_url
                )));
            }
        }
    }

    Ok(())
}

fn validate_filename(pkg: &RepoPackage) -> PicaResult<()> {
    let filename = &pkg.filename;
    if filename.contains('/') || filename.contains("..") || !filename.ends_with(".pkg.tar.gz") {
        return Err(PicaError::msg(format!(
            "package {}: invalid filename {}",
            pkg.pkgname, filename
        )));
    }

    let expected = expected_filename(
        &pkg.pkgname,
        &pkg.pkgver,
        &pkg.pkgrel,
        &pkg.platform,
        &pkg.arch,
    );
    if expected != *filename {
        return Err(PicaError::msg(format!(
            "package {}: filename mismatch, expected {}, got {}",
            pkg.pkgname, expected, filename
        )));
    }

    Ok(())
}

pub fn expected_filename(
    pkgname: &str,
    pkgver: &str,
    pkgrel: &str,
    platform: &str,
    arch: &str,
) -> String {
    if platform == "all" {
        format!("{pkgname}-{pkgver}-{pkgrel}-{arch}.pkg.tar.gz")
    } else {
        format!("{pkgname}-{pkgver}-{pkgrel}-{platform}-{arch}.pkg.tar.gz")
    }
}

pub fn is_supported_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://") || value.starts_with("file://")
}

#[cfg(test)]
mod tests {
    use super::{expected_filename, parse_repo_json};

    #[test]
    fn expected_filename_for_all_platform() {
        assert_eq!(
            expected_filename("hello", "1.0.0", "1", "all", "all"),
            "hello-1.0.0-1-all.pkg.tar.gz"
        );
    }

    #[test]
    fn parse_valid_repo() {
        let input = r#"
        {
          "schema": 1,
          "packages": [
            {
              "pkgname": "hello",
              "pkgver": "1.0.0",
              "pkgrel": "1",
              "platform": "all",
              "arch": "all",
              "filename": "hello-1.0.0-1-all.pkg.tar.gz"
            }
          ]
        }
        "#;

        let parsed = parse_repo_json(input).expect("valid repo");
        assert_eq!(parsed.packages.len(), 1);
    }

    #[test]
    fn reject_invalid_filename() {
        let input = r#"
        {
          "schema": 1,
          "packages": [
            {
              "pkgname": "hello",
              "pkgver": "1.0.0",
              "pkgrel": "1",
              "platform": "all",
              "arch": "all",
              "filename": "../hello.pkg.tar.gz"
            }
          ]
        }
        "#;

        assert!(parse_repo_json(input).is_err());
    }
}
