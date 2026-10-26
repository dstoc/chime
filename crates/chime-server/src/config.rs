use anyhow::{Result, bail};
use clap::Parser;
use std::{path::PathBuf, str::FromStr};
use url::{Host, Url};

#[derive(Debug, Parser)]
#[command(
    name = "chime-server",
    about = "Run the Chime notification service",
    long_about = "Run the Chime service behind its configured HTTPS origin. The server listens on 127.0.0.1:8080."
)]
pub struct Args {
    /// Exact public HTTPS origin used by browsers, for example https://chime.example-tailnet.ts.net
    #[arg(long, value_name = "HTTPS_ORIGIN")]
    origin: Origin,

    /// Private directory for SQLite state and persistent service keys
    #[arg(long, value_name = "PATH")]
    data_dir: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Origin {
    serialized: String,
    rp_id: String,
}

impl Origin {
    pub fn as_str(&self) -> &str {
        &self.serialized
    }

    pub fn rp_id(&self) -> &str {
        &self.rp_id
    }
}

impl FromStr for Origin {
    type Err = anyhow::Error;

    fn from_str(input: &str) -> Result<Self> {
        let url = Url::parse(input)
            .map_err(|_| anyhow::anyhow!("origin must be an absolute HTTPS origin"))?;
        if url.scheme() != "https" {
            bail!("origin must use HTTPS");
        }
        if !url.username().is_empty() || url.password().is_some() {
            bail!("origin must not contain user information");
        }
        if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
            bail!("origin must not contain a path, query, or fragment");
        }

        let host = match url.host() {
            Some(Host::Domain(host)) if !host.is_empty() && !host.ends_with('.') => host,
            Some(Host::Ipv4(_)) | Some(Host::Ipv6(_)) => {
                bail!("origin must use a DNS hostname, not an IP address")
            }
            _ => bail!("origin must include a valid DNS hostname"),
        };

        Ok(Self {
            serialized: url.origin().ascii_serialization(),
            rp_id: host.to_owned(),
        })
    }
}

pub struct Config {
    pub origin: Origin,
    pub data_dir: PathBuf,
}

impl From<Args> for Config {
    fn from(args: Args) -> Self {
        Self {
            origin: args.origin,
            data_dir: args.data_dir,
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        if self.data_dir.as_os_str().is_empty() {
            bail!("--data-dir must not be empty");
        }
        if self.data_dir.exists() && !self.data_dir.is_dir() {
            bail!("--data-dir must name a directory");
        }
        if std::fs::symlink_metadata(&self.data_dir)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            bail!("--data-dir must not be a symbolic link");
        }
        Ok(())
    }

    pub fn checked(args: Args) -> Result<Self> {
        let config = Self::from(args);
        config.validate()?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::{Args, Config, Origin};
    use clap::Parser;
    use std::{path::PathBuf, str::FromStr};

    #[test]
    fn https_origin_derives_the_rp_id_from_its_hostname() {
        let origin = Origin::from_str("https://chime.example-tailnet.ts.net:8443/").unwrap();

        assert_eq!(origin.as_str(), "https://chime.example-tailnet.ts.net:8443");
        assert_eq!(origin.rp_id(), "chime.example-tailnet.ts.net");
    }

    #[test]
    fn rejects_non_https_or_non_origin_urls() {
        for input in [
            "http://chime.example.ts.net",
            "https://user@chime.example.ts.net",
            "https://chime.example.ts.net/admin",
            "https://chime.example.ts.net?x=1",
            "https://chime.example.ts.net#fragment",
            "https://127.0.0.1",
            "not a URL",
        ] {
            assert!(Origin::from_str(input).is_err(), "accepted {input}");
        }
    }

    #[test]
    fn command_requires_origin_and_data_directory() {
        assert!(Args::try_parse_from(["chime-server"]).is_err());
        assert!(
            Args::try_parse_from([
                "chime-server",
                "--origin",
                "http://chime.example.ts.net",
                "--data-dir",
                "/tmp/chime"
            ])
            .is_err()
        );
    }

    #[test]
    fn rejects_empty_or_non_directory_data_path() {
        let args = Args::try_parse_from([
            "chime-server",
            "--origin",
            "https://chime.example.ts.net",
            "--data-dir",
            "",
        ])
        .unwrap();
        let mut config = Config::from(args);
        assert!(config.validate().is_err());

        let file = tempfile::NamedTempFile::new().unwrap();
        config.data_dir = PathBuf::from(file.path());
        assert!(config.validate().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symbolic_link_data_directory() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = directory.path().join("link");
        symlink(&target, &link).unwrap();

        let args = Args::try_parse_from([
            "chime-server",
            "--origin",
            "https://chime.example.ts.net",
            "--data-dir",
            link.to_str().unwrap(),
        ])
        .unwrap();
        assert!(Config::checked(args).is_err());
    }
}
