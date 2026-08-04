#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use anyhow::Result;
use clap::Parser;
use remux_server::{Config, FilesystemPaths, serve, setup_logging};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Remux media server")]
struct Cli {
    #[arg(long, help = "Data directory")]
    datadir: Option<PathBuf>,
    #[arg(long, help = "HTTP port")]
    port: Option<u16>,
    #[arg(long, help = "SQLite database URL")]
    database_url: Option<String>,
    #[arg(long, help = "Path to ffmpeg binary")]
    ffmpeg: Option<PathBuf>,
    #[arg(long, help = "Path to ffprobe binary")]
    ffprobe: Option<PathBuf>,
}

#[derive(Deserialize)]
struct CliConfig {
    #[serde(flatten)]
    base: Config,
    #[serde(flatten)]
    paths: FilesystemPaths,
}

fn load_cli_config(
    cfg: &str,
    env: config::Environment,
) -> Result<CliConfig, config::ConfigError> {
    config::Config::builder()
        .add_source(config::File::with_name(cfg).required(false))
        .add_source(env.try_parsing(true))
        .build()?
        .try_deserialize()
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();

    // Bootstrap ffmpeg paths before Config loads (they're read as bare env vars).
    if let Some(p) = &cli.ffmpeg {
        unsafe { std::env::set_var("FFMPEG_PATH", p) };
    }
    if let Some(p) = &cli.ffprobe {
        unsafe { std::env::set_var("FFPROBE_PATH", p) };
    }

    let cfg = std::env::var("CONFIG").unwrap_or_else(|_| "/data/config".to_string());
    let cli_config = load_cli_config(&cfg, config::Environment::default())?;
    let mut config = cli_config.base;

    // CLI args win over env.
    if let Some(v) = cli.datadir {
        config.data_dir = v;
    }
    if let Some(v) = cli.port {
        config.port = v;
    }
    if let Some(v) = cli.database_url {
        config.database_url = Some(v);
    }

    let config = config.resolve();
    // Logging starts after config and CLI resolution so file logs use the
    // final directory; retain the guard through the server lifetime.
    let _log_guard = setup_logging(
        config
            .log_dir
            .as_deref()
            .map(std::path::Path::new),
    );
    serve(config, cli_config.paths).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_port_from_string_environment_value() {
        let env = config::Environment::default().source(Some({
            let mut env = config::Map::new();
            env.insert("PORT".into(), "5000".into());
            env
        }));

        let config = load_cli_config("/tmp/remux-missing-test-config", env).unwrap();

        assert_eq!(
            config
                .base
                .port,
            5000
        );
    }
}
