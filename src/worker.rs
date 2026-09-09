//! Single-owner BLE worker: serializes jobs from the API onto one connection,
//! with job-level retry (reconnect + resend) for the printer's flaky BLE.

use tokio::sync::{mpsc, oneshot};

use crate::ble::{Ble, DeviceInfo, Status};

pub enum Job {
    Print { data: Vec<u8>, resp: oneshot::Sender<Result<(), String>> },
    Status { resp: oneshot::Sender<Result<Status, String>> },
    Scan { secs: u64, resp: oneshot::Sender<Result<Vec<DeviceInfo>, String>> },
}

pub type Sender = mpsc::Sender<Job>;

pub fn channel() -> (Sender, mpsc::Receiver<Job>) {
    mpsc::channel(32)
}

pub async fn run(mut ble: Ble, mut rx: mpsc::Receiver<Job>) {
    while let Some(job) = rx.recv().await {
        match job {
            Job::Print { data, resp } => {
                let _ = resp.send(print_with_retry(&mut ble, &data).await);
            }
            Job::Status { resp } => {
                let _ = resp.send(ble.query_status().await.map_err(|e| e.to_string()));
            }
            Job::Scan { secs, resp } => {
                let _ = resp.send(ble.scan(secs).await.map_err(|e| e.to_string()));
            }
        }
    }
}

async fn print_with_retry(ble: &mut Ble, data: &[u8]) -> Result<(), String> {
    let mut last = String::from("unknown");
    for attempt in 1..=3 {
        match ble.send(data).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = e.to_string();
                tracing::warn!("print attempt {attempt}/3 failed: {last}");
                let _ = ble.disconnect().await; // force a clean reconnect next try
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
    Err(last)
}
