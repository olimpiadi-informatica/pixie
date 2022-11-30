mod pull;
mod push;
mod register;

use anyhow::{bail, Result};
use std::env;

fn main() -> Result<()> {
    match env::args().next().as_ref().map(|x| x.as_ref()) {
        Some("pixie-push") => push::main(),
        Some("pixie-pull") => pull::main(),
        Some("pixie-register") => register::main(),
        _ => bail!("Invalid program name"),
    }
}
