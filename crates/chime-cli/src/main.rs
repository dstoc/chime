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
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};
use url::Url;

const MAX_API_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

#[derive(Debug, Parser)]
#[command(
    name = "chime",
    about = "Send notifications with Chime",
    after_help = "Examples:\n  chime send --title 'Build completed' --body 'All checks passed'\n  chime send --title 'Render ready' --image ./render.png --url https://example.com/result\n  chime send --title 'Build completed' --image-url https://example.com/build.png"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Send a notification to all active receiving devices.
    Send {
        /// Short notification title.
        #[arg(long)]
        title: String,
        /// Optional notification body.
        #[arg(long)]
        body: Option<String>,
        /// Upload a PNG, JPEG, or WebP image.
        #[arg(long, value_name = "PATH", conflicts_with = "image_url")]
        image: Option<PathBuf>,
        /// Use an absolute HTTPS image URL. Chime does not fetch this URL.
        #[arg(long, value_name = "HTTPS_URL", conflicts_with = "image")]
        image_url: Option<String>,
        /// Open an absolute HTTPS URL when the notification is clicked.
        #[arg(long, value_name = "HTTPS_URL")]
        url: Option<String>,
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
            image,
            image_url,
            url,
            config,
        } => send(title, body, image, image_url, url, config),
    }
}

fn send(
    title: String,
    body: Option<String>,
    image: Option<PathBuf>,
    image_url: Option<String>,
    url: Option<String>,
    path: Option<PathBuf>,
) -> Result<ExitCode> {
    if image.is_some() && image_url.is_some() {
        bail!("use --image or --image-url, not both");
    }
    if let Some(value) = &image_url {
        validate_https_url(value)
            .context("--image-url must be an absolute HTTPS URL without user information")?;
    }
    if let Some(value) = &url {
        validate_https_url(value)
            .context("--url must be an absolute HTTPS URL without user information")?;
    }
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

    let request = NotificationRequest {
        title: title.clone(),
        body: body.clone(),
        image_url: image_url.clone(),
        url: url.clone(),
    };
    let (request_body, content_type) = if let Some(path) = image {
        let (image_bytes, image_type) = read_image(&path)?;
        multipart_body(
            &title,
            body.as_deref(),
            image_url.as_deref(),
            url.as_deref(),
            &image_bytes,
            image_type,
        )?
    } else {
        (serde_json::to_vec(&request)?, "application/json".to_owned())
    };
    let client = HttpClient::builder()
        .timeout(Duration::from_secs(100))
        .redirect_policy(RedirectPolicy::None)
        .proxy(None)
        .build()
        .context("create HTTPS client")?;
    let request = Request::post(format!("{origin}/api/notify"))
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", content_type)
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

fn read_image(path: &Path) -> Result<(Vec<u8>, &'static str)> {
    let file = File::open(path).with_context(|| format!("open image {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take((MAX_IMAGE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read image {}", path.display()))?;
    if bytes.len() > MAX_IMAGE_BYTES {
        bail!("image exceeds the 5 MiB upload limit");
    }
    let content_type = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        bail!("image must have a PNG, JPEG, or WebP file signature");
    };
    if bytes.is_empty() {
        bail!("image file is empty");
    }
    Ok((bytes, content_type))
}

fn multipart_body(
    title: &str,
    body: Option<&str>,
    image_url: Option<&str>,
    url: Option<&str>,
    image: &[u8],
    image_type: &'static str,
) -> Result<(Vec<u8>, String)> {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

    let mut random = [0_u8; 32];
    getrandom::fill(&mut random).context("generate multipart boundary")?;
    let boundary = format!("chime-{}", URL_SAFE_NO_PAD.encode(random));
    let mut output = Vec::with_capacity(image.len() + 4096);
    append_text_part(&mut output, &boundary, "title", title);
    if let Some(body) = body {
        append_text_part(&mut output, &boundary, "body", body);
    }
    if let Some(image_url) = image_url {
        append_text_part(&mut output, &boundary, "image_url", image_url);
    }
    if let Some(url) = url {
        append_text_part(&mut output, &boundary, "url", url);
    }
    output.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"image-upload\"\r\nContent-Type: {image_type}\r\n\r\n").as_bytes(),
    );
    output.extend_from_slice(image);
    output.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Ok((output, format!("multipart/form-data; boundary={boundary}")))
}

fn append_text_part(output: &mut Vec<u8>, boundary: &str, name: &str, value: &str) {
    output.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n")
            .as_bytes(),
    );
    output.extend_from_slice(value.as_bytes());
    output.extend_from_slice(b"\r\n");
}

fn validate_https_url(value: &str) -> Result<()> {
    let url = Url::parse(value).context("invalid URL")?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("URL must use HTTPS and must not contain user information");
    }
    Ok(())
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
    use super::{Args, validate_api_key, validate_https_url, validate_server_url};
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use clap::Parser;

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

    #[test]
    fn send_accepts_image_and_click_options_but_rejects_both_image_sources() {
        let args = Args::try_parse_from([
            "chime",
            "send",
            "--title",
            "Render ready",
            "--image",
            "render.png",
            "--url",
            "https://example.com/result",
        ])
        .unwrap();
        assert!(matches!(
            args.command,
            super::Command::Send {
                image: Some(_),
                url: Some(_),
                ..
            }
        ));
        assert!(
            Args::try_parse_from([
                "chime",
                "send",
                "--title",
                "Render ready",
                "--image",
                "render.png",
                "--image-url",
                "https://example.com/render.png",
            ])
            .is_err()
        );
    }

    #[test]
    fn image_and_click_urls_require_absolute_https_without_credentials() {
        assert!(validate_https_url("https://example.com/result").is_ok());
        for value in [
            "http://example.com/image.png",
            "https://user@example.com/image.png",
            "/relative/image.png",
        ] {
            assert!(validate_https_url(value).is_err(), "accepted {value}");
        }
    }
}
