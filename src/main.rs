//! thermorinterd — cross-platform BLE cat-printer daemon (HTTP print service).

mod api;
mod ble;
mod protocol;
mod render;
mod worker;

use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio::net::TcpListener;

use ble::Ble;

#[derive(Parser)]
#[command(name = "thermorinterd", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the HTTP print service (default).
    Serve {
        #[arg(long, default_value = "127.0.0.1:9100")]
        addr: String,
        /// Device name substring or id to connect to (default: auto-detect).
        #[arg(long)]
        device: Option<String>,
        /// TrueType font path for text/QR (default: search system fonts).
        #[arg(long)]
        font: Option<String>,
        #[arg(long, default_value_t = 12288)]
        energy: u16,
        #[arg(long, default_value_t = 80)]
        feed: u16,
    },
    /// Scan for nearby BLE devices.
    Scan {
        #[arg(long, default_value_t = 6)]
        secs: u64,
    },
    /// Query device firmware / state.
    Status {
        #[arg(long)]
        device: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Cmd::Serve {
        addr: "127.0.0.1:9100".into(),
        device: None,
        font: None,
        energy: 12288,
        feed: 80,
    }) {
        Cmd::Serve { addr, device, font, energy, feed } => {
            serve(addr, device, font, energy, feed).await
        }
        Cmd::Scan { secs } => {
            let ble = Ble::new(None).await?;
            for d in ble.scan(secs).await? {
                let star = if d.likely_printer { "★" } else { " " };
                println!("{star} {:<24} id={}  rssi={:?}", d.name, d.id, d.rssi);
            }
            Ok(())
        }
        Cmd::Status { device } => {
            let mut ble = Ble::new(device).await?;
            let s = ble.query_status().await?;
            println!("{}", serde_json::to_string_pretty(&s)?);
            Ok(())
        }
    }
}

async fn serve(
    addr: String,
    device: Option<String>,
    font: Option<String>,
    energy: u16,
    feed: u16,
) -> Result<()> {
    let ble = Ble::new(device).await?;
    let (tx, rx) = worker::channel();
    tokio::spawn(worker::run(ble, rx));

    let font = match render::load_font(font.as_deref()) {
        Ok(f) => Some(f),
        Err(e) => {
            tracing::warn!("no font loaded ({e}); /print/text and /print/qr will be unavailable");
            None
        }
    };

    let state = api::AppState { tx, font: Arc::new(font), energy, feed };
    let app = api::router(state);

    let listener = TcpListener::bind(&addr).await?;
    tracing::info!("thermorinterd listening on http://{addr}");
    tracing::info!("endpoints: GET /health /status /scan  POST /print/{{text,qr,image,raw}} /feed");
    axum::serve(listener, app).await?;
    Ok(())
}
