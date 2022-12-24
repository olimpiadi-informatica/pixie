use std::{fs::File, path::PathBuf, sync::Arc, sync::Mutex};

use anyhow::{Context, Result};
use clap::Parser;

use pixie_shared::{Config, PersistentServerState, Station, Unit};

use pixie_core::{dnsmasq::DnsmasqHandle, http, udp, State};

#[derive(Parser, Debug)]
pub struct PixieOptions {
    /// Directory in which files will be stored.
    /// Must already contain files: tftpboot/uefi_app.efi, config.yaml
    #[clap(short, long, default_value = "./storage")]
    storage_dir: PathBuf,
}

#[actix_rt::main]
async fn main() -> Result<()> {
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

    for file_path in [["tftpboot", "uefi_app.efi"]] {
        let mut path = options.storage_dir.clone();
        for path_piece in file_path {
            path = path.join(path_piece);
        }
        anyhow::ensure!(path.is_file(), "{} not found", path.display());
    }

    let config_path = options.storage_dir.join("config.yaml");
    let config = File::open(&config_path)
        .with_context(|| format!("open config file: {}", config_path.display()))?;
    let config: Config = serde_yaml::from_reader(&config)
        .with_context(|| format!("deserialize config from {}", config_path.display()))?;

    let mut dnsmasq_handle = DnsmasqHandle::from_config(&options.storage_dir, &config.dhcp)
        .context("Error start dnsmasq")?;

    let data = std::fs::read(options.storage_dir.join("registered.json"));
    let units: Vec<Unit> = data
        .ok()
        .map(|d| serde_json::from_slice(&d))
        .transpose()
        .context("invalid json at registered.json")?
        .unwrap_or_default();

    dnsmasq_handle.set_hosts(&units)?;

    let last = Mutex::new(Station {
        image: config.images[0].clone(),
        ..Default::default()
    });

    let state = Arc::new(State {
        storage_dir: options.storage_dir,
        persistent: Mutex::new(PersistentServerState { config, units }),
        dnsmasq_handle: Mutex::new(dnsmasq_handle),
        last,
    });

    tokio::select!(
        x = http::main(state.clone()) => x?,
        x = udp::main(&state) => x?,
    );

    Ok(())
}
