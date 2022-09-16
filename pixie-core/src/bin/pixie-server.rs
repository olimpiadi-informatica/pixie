use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use pixie_core::http;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct PixieConfig {
    dnsmasq: pixie_core::dnsmasq::Config,
    http: pixie_core::http::Config,
    boot: pixie_core::http::BootConfig,
}

#[derive(Parser, Debug)]
pub struct PixieOptions {
    /// Folder in which files will be stored. Must already contain a tftpboot/ipxe.efi file.
    #[clap(short, long, default_value = "./storage")]
    storage_dir: PathBuf,
}

fn main() -> Result<()> {
    env_logger::init();

    // Validate the configuration.
    let mut options = PixieOptions::parse();

    std::fs::create_dir_all(&options.storage_dir)
        .with_context(|| format!("create storage dir: {}", options.storage_dir.display()))?;

    options.storage_dir = std::fs::canonicalize(&options.storage_dir)
        .with_context(|| format!("storage dir is invalid: {}", options.storage_dir.display()))?;

    anyhow::ensure!(
        options.storage_dir.to_str().is_some(),
        "storage dir must be valid utf8"
    );

    for file_path in [["tftpboot", "ipxe.efi"], ["httpstatic", "reboot.efi"]] {
        let mut path = options.storage_dir.clone();
        for path_piece in file_path {
            path = path.join(path_piece);
        }
        anyhow::ensure!(path.is_file(), "{} not found", path.display());
    }

    let config_path = options.storage_dir.join("config.yaml");
    let config = std::fs::File::open(&config_path)
        .with_context(|| format!("open config file: {}", config_path.display()))?;
    let config: PixieConfig = serde_yaml::from_reader(&config)
        .with_context(|| format!("deserialize config from {}", config_path.display()))?;

    println!(
        "{}",
        config
            .dnsmasq
            .to_dnsmasq_config(&options.storage_dir, &config.http)
            .with_context(|| "Error generating dnsmasq config")?
    );

    http::main_sync(options.storage_dir, config.http, config.boot)?;

    Ok(())
}
