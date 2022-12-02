use std::fs::{read, read_to_string, File};

use anyhow::{ensure, Result};
use clap::Parser;
use serde_derive::{Deserialize, Serialize};

#[derive(Parser, Debug)]
struct Options {
    #[clap(short, long, value_parser)]
    boot_order_path: String,
}

fn current_boot_options() -> Result<Vec<u16>> {
    let current_bo =
        read("/sys/firmware/efi/efivars/BootOrder-8be4df61-93ca-11d2-aa0d-00e098032b8c")?;
    current_bo[4..]
        .chunks(2)
        .map(|x| Ok(u16::from_le_bytes(x.try_into()?)))
        .collect()
}

fn set_boot_options(bo: Vec<u16>) -> Result<()> {
    let bo = b"\x07\0\0\0"
        .into_iter()
        .copied()
        .chain(bo.into_iter().flat_map(|x| x.to_le_bytes().into_iter()))
        .collect::<Vec<_>>();
    std::fs::write(
        "/sys/firmware/efi/efivars/BootOrder-8be4df61-93ca-11d2-aa0d-00e098032b8c",
        &bo,
    )?;
    Ok(())
}

fn read_boot_option(opt: u16) -> Result<Vec<u8>> {
    Ok(read(&format!(
        "/sys/firmware/efi/efivars/Boot{:04X}-8be4df61-93ca-11d2-aa0d-00e098032b8c",
        opt
    ))?)
}

fn write_boot_option(opt: u16, data: &Vec<u8>) -> Result<()> {
    Ok(std::fs::write(
        &format!(
            "/sys/firmware/efi/efivars/Boot{:04X}-8be4df61-93ca-11d2-aa0d-00e098032b8c",
            opt
        ),
        data,
    )?)
}

#[derive(Debug, Serialize, Deserialize)]
struct BootOrder {
    first_option: (u16, Vec<u8>),
    second_option: (u16, Vec<u8>),
}

pub fn save_boot_order() -> Result<()> {
    let args = Options::parse();
    ensure!(!args.boot_order_path.is_empty(), "Specify a source");
    let boot_options = current_boot_options()?;

    let boot_order = BootOrder {
        first_option: (boot_options[0], read_boot_option(boot_options[0])?),
        second_option: (boot_options[1], read_boot_option(boot_options[1])?),
    };

    Ok(serde_json::to_writer(
        File::create(args.boot_order_path)?,
        &boot_order,
    )?)
}

pub fn set_boot_order() -> Result<()> {
    let args = Options::parse();
    ensure!(!args.boot_order_path.is_empty(), "Specify a source");

    let boot_order: BootOrder = serde_json::from_str(&read_to_string(args.boot_order_path)?)?;

    write_boot_option(boot_order.first_option.0, &boot_order.first_option.1)?;
    write_boot_option(boot_order.second_option.0, &boot_order.second_option.1)?;

    let boot_options = current_boot_options()?;
    let opts = [boot_order.first_option.0, boot_order.second_option.0]
        .into_iter()
        .chain(
            boot_options
                .into_iter()
                .filter(|x| *x != boot_order.first_option.0 && *x != boot_order.second_option.0),
        )
        .collect::<Vec<_>>();

    set_boot_options(opts)
}
