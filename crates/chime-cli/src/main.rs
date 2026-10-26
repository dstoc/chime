use anyhow::{Context, Result, bail};
use chime_protocol::{DeliveryOutcome, DeliverySummary, NotificationRequest};
use clap::{Parser, Subcommand};
use isahc::{
    HttpClient,
    config::{Configurable, RedirectPolicy},
    http::Request,
};
use serde::Deserialize;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};
use url::Url;

const MAX_API_RESPONSE_BYTES: usize = 16 * 1024;

#[derive(Debug, Parser)]
#[command(name = "chime", about = "Send text notifications with Chime")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Send a text notification to all active receiving devices.
    Send {
        /// Short notification title.
        #[arg(long)]
        title: String,
        /// Optional notification body.
        #[arg(long)]
        body: Option<String>,
        /// Read server URL and send key from this protected JSON file.
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
    },
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    server_url: Option<String>,
    api_key: Option<String>,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("chime: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let Args { command } = Args::parse();
    match command {
        Command::Send {
            title,
            body,
            config,
        } => send(title, body, config),
    }
}

fn send(title: String, body: Option<String>, path: Option<PathBuf>) -> Result<ExitCode> {
    let file_config = match (
        std::env::var_os("CHIME_API_KEY"),
        std::env::var_os("CHIME_URL"),
    ) {
        (Some(_), Some(_)) => FileConfig::default(),
        _ => read_config(&config_path(path))?,
    };
    let server_url = std::env::var("CHIME_URL")
        .ok()
        .or(file_config.server_url)
        .context("set CHIME_URL or add server_url to the Chime config file")?;
    let api_key = std::env::var("CHIME_API_KEY")
        .ok()
        .or(file_config.api_key)
        .context("set CHIME_API_KEY or add api_key to the Chime config file")?;
    validate_api_key(&api_key)?;
    let origin = validate_server_url(&server_url)?;

    let request_body = serde_json::to_vec(&NotificationRequest { title, body })?;
    let client = HttpClient::builder()
        .timeout(Duration::from_secs(100))
        .redirect_policy(RedirectPolicy::None)
        .proxy(None)
        .build()
        .context("create HTTPS client")?;
    let request = Request::post(format!("{origin}/api/notify"))
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .body(request_body)
        .context("create notification request")?;
    let response = client.send(request).context("send notification request")?;
    let status = response.status();
    let mut response_body = Vec::new();
    let body_size = response
        .into_body()
        .take((MAX_API_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut response_body)
        .context("read Chime response")?;
    if body_size > MAX_API_RESPONSE_BYTES {
        bail!("Chime returned an oversized response");
    }
    if !status.is_success() {
        #[derive(Deserialize)]
        struct ApiError {
            error: Option<String>,
        }
        let message = serde_json::from_slice::<ApiError>(&response_body)
            .ok()
            .and_then(|body| body.error)
            .unwrap_or_else(|| "Chime rejected the send request.".to_owned());
        bail!("HTTP {status}: {message}");
    }
    let summary: DeliverySummary =
        serde_json::from_slice(&response_body).context("parse Chime delivery summary")?;
    println!(
        "Provider acceptance: {}/{} accepted, {} failed, {} expired subscriptions removed ({:?}). This does not confirm device receipt.",
        summary.accepted,
        summary.attempted,
        summary.failed,
        summary.expired_subscriptions_removed,
        summary.outcome
    );
    Ok(if summary.outcome == DeliveryOutcome::Accepted {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn config_path(explicit: Option<PathBuf>) -> PathBuf {
    explicit
        .or_else(|| std::env::var_os("CHIME_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| {
            let base = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(|user_home| PathBuf::from(user_home).join(".config"))
                })
                .unwrap_or_else(|| PathBuf::from("."));
            base.join("chime/config.json")
        })
}

fn read_config(path: &Path) -> Result<FileConfig> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FileConfig::default());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("read config metadata {}", path.display()));
        }
    };
    if !metadata.is_file() {
        bail!("config path must be a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("config file must not be readable or writable by group or other users");
        }
    }
    let content = fs::read(path).with_context(|| format!("read config file {}", path.display()))?;
    serde_json::from_slice(&content).context("parse Chime config file")
}

fn validate_api_key(api_key: &str) -> Result<()> {
    let encoded = api_key
        .strip_prefix("chime_sk_")
        .context("send key must be a Chime send key")?;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .context("send key is malformed")?;
    if bytes.len() != 32 || URL_SAFE_NO_PAD.encode(bytes) != encoded {
        bail!("send key is malformed");
    }
    Ok(())
}

fn validate_server_url(value: &str) -> Result<String> {
    let url = Url::parse(value).context("CHIME_URL must be an HTTPS origin")?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port().is_some_and(|port| port != 443)
    {
        bail!("CHIME_URL must be an HTTPS origin without credentials, path, query, or fragment");
    }
    Ok(url.origin().ascii_serialization())
}

#[cfg(test)]
mod tests {
    use super::{validate_api_key, validate_server_url};
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

    #[test]
    fn validates_https_origin_and_send_key() {
        assert_eq!(
            validate_server_url("https://chime.example.ts.net/").unwrap(),
            "https://chime.example.ts.net"
        );
        let key = format!("chime_sk_{}", URL_SAFE_NO_PAD.encode([8_u8; 32]));
        assert!(validate_api_key(&key).is_ok());
        for url in [
            "http://chime.example.ts.net",
            "https://user@chime.example.ts.net",
            "https://chime.example.ts.net/api",
            "https://chime.example.ts.net?x=1",
        ] {
            assert!(validate_server_url(url).is_err());
        }
        assert!(validate_api_key("wrong").is_err());
    }
}
