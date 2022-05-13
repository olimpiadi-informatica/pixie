use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use crate::dnsmasq::{Config, Net};

mod dnsmasq;

#[derive(Parser, Debug)]
pub struct PixieOptions {
    /// Folder in which files will be stored. Must already contain a tftpboot/ipxe.efi file.
    #[clap(short, long)]
    storage_dir: PathBuf,
}

fn main() -> Result<()> {
    // Validate the configuration.

    let mut options = PixieOptions::parse();

    options.storage_dir = std::fs::canonicalize(&options.storage_dir).with_context(|| {
        format!(
            "storage dir is invalid: {}",
            options.storage_dir.to_string_lossy()
        )
    })?;

    anyhow::ensure!(
        options.storage_dir.to_str().is_some(),
        "storage dir must be valid utf8"
    );

    anyhow::ensure!(
        options
            .storage_dir
            .join("tftpboot")
            .join("ipxe.efi")
            .is_file(),
        "tftpboot/ipxe.efi does not exist in the storage directory"
    );

    let config = Config {
        networks: vec![Net {
            dhcp_config: None,
            interface: "enp67s0f0".into(),
        }],
    };

    println!(
        "{}",
        config
            .to_dnsmasq_config(&options.storage_dir)
            .with_context(|| "Error generating dnsmasq config")?
    );

    Ok(())
}
