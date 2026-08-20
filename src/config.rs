use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub bind: SocketAddr,
    pub image_path: PathBuf,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read configuration from {}", path.display()))?;
        let config: Self = toml::from_str(&contents)
            .with_context(|| format!("failed to parse configuration from {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.image_path.as_os_str().is_empty() {
            bail!("image_path must not be empty");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn loads_valid_configuration() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "bind = \"127.0.0.1:8080\"\nimage_path = \"images/current.jpg\"\n",
        )
        .unwrap();

        let config = Config::load(&path).unwrap();

        assert_eq!(config.bind, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(config.image_path, PathBuf::from("images/current.jpg"));
    }

    #[test]
    fn rejects_invalid_toml() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "not valid toml").unwrap();

        let error = Config::load(&path).unwrap_err().to_string();

        assert!(error.contains("failed to parse configuration"));
    }

    #[test]
    fn rejects_empty_image_path() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "bind = \"127.0.0.1:8080\"\nimage_path = \"\"\n").unwrap();

        let error = Config::load(&path).unwrap_err().to_string();

        assert!(error.contains("image_path must not be empty"));
    }

    #[test]
    fn reports_missing_configuration() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("missing.toml");

        let error = Config::load(&path).unwrap_err().to_string();

        assert!(error.contains("failed to read configuration"));
    }
}
