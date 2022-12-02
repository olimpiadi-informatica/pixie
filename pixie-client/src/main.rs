mod boot_order;
mod pull;
mod push;
mod register;

use anyhow::{bail, Result};
use std::env;

fn main() -> Result<()> {
    match env::args().next().as_ref().map(|x| x.as_ref()) {
        Some("pixie-save-boot-order") => boot_order::save_boot_order(),
        Some("pixie-set-boot-order") => boot_order::set_boot_order(),
        Some("pixie-push") => push::main(),
        Some("pixie-pull") => pull::main(),
        Some("pixie-register") => register::main(),
        _ => bail!("Invalid program name"),
    }
}
