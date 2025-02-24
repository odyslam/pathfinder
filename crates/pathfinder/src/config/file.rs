//! TOML configuration file parsing
use serde::Deserialize;

use crate::config::builder::ConfigBuilder;

#[derive(Deserialize, Debug, PartialEq)]
struct EthereumConfig {
    url: Option<String>,
    user: Option<String>,
    password: Option<String>,
}

#[derive(Deserialize, Debug, PartialEq)]
struct FileConfig {
    ethereum: Option<EthereumConfig>,
    #[serde(rename = "http-rpc")]
    http_rpc: Option<String>,
}

impl FileConfig {
    fn into_config_options(self) -> ConfigBuilder {
        use crate::config::ConfigOption;
        let builder = match self.ethereum {
            Some(eth) => ConfigBuilder::default()
                .with(ConfigOption::EthereumHttpUrl, eth.url)
                .with(ConfigOption::EthereumUser, eth.user)
                .with(ConfigOption::EthereumPassword, eth.password),
            None => ConfigBuilder::default(),
        };
        builder.with(ConfigOption::HttpRpcAddress, self.http_rpc)
    }
}

/// Parses a [ConfigBuilder] from a toml format file.
pub fn config_from_filepath(filepath: &std::path::Path) -> std::io::Result<ConfigBuilder> {
    let file_contents = std::fs::read_to_string(filepath)?;
    config_from_str(&file_contents)
}

fn config_from_str(s: &str) -> std::io::Result<ConfigBuilder> {
    toml::from_str::<FileConfig>(s)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()))
        .map(|cfg| cfg.into_config_options())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigOption;

    #[test]
    fn ethereum_url() {
        let value = "value".to_owned();
        let toml = format!(r#"ethereum.url = "{}""#, value);
        let mut cfg = config_from_str(&toml).unwrap();
        assert_eq!(cfg.take(ConfigOption::EthereumHttpUrl), Some(value));
    }

    #[test]
    fn ethereum_user() {
        let value = "value".to_owned();
        let toml = format!(r#"ethereum.user = "{}""#, value);
        let mut cfg = config_from_str(&toml).unwrap();
        assert_eq!(cfg.take(ConfigOption::EthereumUser), Some(value));
    }

    #[test]
    fn ethereum_password() {
        let value = "value".to_owned();
        let toml = format!(r#"ethereum.password = "{}""#, value);
        let mut cfg = config_from_str(&toml).unwrap();
        assert_eq!(cfg.take(ConfigOption::EthereumPassword), Some(value));
    }

    #[test]
    fn ethereum_section() {
        let user = "user".to_owned();
        let url = "url".to_owned();
        let password = "password".to_owned();

        let toml = format!(
            r#"[ethereum]
user = "{}"
url = "{}"
password = "{}""#,
            user, url, password
        );

        let mut cfg = config_from_str(&toml).unwrap();
        assert_eq!(cfg.take(ConfigOption::EthereumUser), Some(user));
        assert_eq!(cfg.take(ConfigOption::EthereumHttpUrl), Some(url));
        assert_eq!(cfg.take(ConfigOption::EthereumPassword), Some(password));
    }

    #[test]
    fn http_rpc() {
        let value = "value".to_owned();
        let toml = format!(r#"http-rpc = "{}""#, value);
        let mut cfg = config_from_str(&toml).unwrap();
        assert_eq!(cfg.take(ConfigOption::HttpRpcAddress), Some(value));
    }

    #[test]
    fn empty_config() {
        let cfg = config_from_str("").unwrap();
        assert_eq!(cfg, ConfigBuilder::default());
    }
}
