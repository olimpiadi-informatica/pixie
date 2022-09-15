use anyhow::{bail, ensure, Result};
use clap::Parser;
use reqwest::blocking::Client;

use pixie_shared::{Station, StationKind};

#[derive(Parser)]
struct Options {
    #[clap(short, long, value_parser)]
    server: String,
    #[clap(short, long, value_parser)]
    kind: String,
    #[clap(short, long, value_parser)]
    row: u32,
    #[clap(short, long, value_parser)]
    col: u32,
}

fn main() -> Result<()> {
    let args = Options::parse();

    let kind = match args.kind.to_lowercase().trim() {
        "w" | "worker" => StationKind::Worker,
        "n" | "non-worker" => StationKind::NonWorker,
        _ => bail!("Invalid kind"),
    };

    let data = Station {
        kind,
        row: args.row,
        col: args.col,
    };

    let body = serde_json::to_string(&data)?;
    let client = Client::new();
    let resp = client
        .post(format!("http://{}/register", args.server))
        .body(body)
        .send()?;
    ensure!(resp.status().is_success(), "status ({}) != 200", resp.status().as_u16());

    Ok(())
}
