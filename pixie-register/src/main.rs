use std::io::{self, BufRead, Write};

use anyhow::{bail, ensure, Result};
use clap::Parser;
use reqwest::blocking::Client;

use pixie_shared::{Station, StationKind};

#[derive(Parser)]
struct Options {
    #[clap(short, long, value_parser)]
    server: String,
}

fn main() -> Result<()> {
    let args = Options::parse();

    let mut stdin = io::stdin().lock();
    let mut stderr = io::stderr().lock();
    let mut buf = String::new();

    let resp = Client::new()
        .get(format!("http://{}/register_hint", args.server))
        .send()?;
    ensure!(
        resp.status().is_success(),
        "status ({}) != 200",
        resp.status().as_u16()
    );

    let hint: Station = serde_json::from_str(&resp.text()?)?;

    write!(stderr, "worker / non-worker? [{:?}] ", hint.kind)?;
    buf.clear();
    stdin.read_line(&mut buf)?;
    let kind = match buf.to_lowercase().trim() {
        "" => hint.kind,
        "w" | "worker" => StationKind::Worker,
        "n" | "non-worker" => StationKind::NonWorker,
        _ => bail!("Invalid kind"),
    };

    write!(stderr, "row? [{}] ", hint.row)?;
    buf.clear();
    stdin.read_line(&mut buf)?;
    let row = match buf.trim() {
        "" => hint.row,
        s => s.parse()?,
    };

    write!(stderr, "col? [{}] ", hint.col)?;
    buf.clear();
    stdin.read_line(&mut buf)?;
    let col = match buf.trim() {
        "" => hint.col,
        s => s.parse()?,
    };

    let data = Station { kind, row, col };

    let body = serde_json::to_string(&data)?;
    let resp = Client::new()
        .post(format!("http://{}/register", args.server))
        .body(body)
        .send()?;
    ensure!(
        resp.status().is_success(),
        "status ({}) != 200",
        resp.status().as_u16()
    );

    Ok(())
}
