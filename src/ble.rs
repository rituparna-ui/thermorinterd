//! Cross-platform BLE transport (btleplug: CoreBluetooth / BlueZ / WinRT).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::StreamExt;
use serde::Serialize;
use uuid::{uuid, Uuid};

use crate::protocol;

const SVC_AE30: Uuid = uuid!("0000ae30-0000-1000-8000-00805f9b34fb");
const SVC_AF30: Uuid = uuid!("0000af30-0000-1000-8000-00805f9b34fb");
const CHAR_AE01: Uuid = uuid!("0000ae01-0000-1000-8000-00805f9b34fb");
const CHAR_AE02: Uuid = uuid!("0000ae02-0000-1000-8000-00805f9b34fb");
const CHAR_AE10: Uuid = uuid!("0000ae10-0000-1000-8000-00805f9b34fb");

const NAME_HINTS: &[&str] = &["td-", "gb01", "gb02", "gt01", "mx", "yy", "cat", "printer"];

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub firmware: Option<String>,
    pub state_raw: Option<String>,
    pub ae10_raw: Option<String>,
    pub notifications: Vec<String>,
    /// Not exposed by the classic 0xAE30 firmware; null unless a model reports it.
    pub battery: Option<u8>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    pub name: String,
    pub id: String,
    pub rssi: Option<i16>,
    pub likely_printer: bool,
}

pub struct Ble {
    adapter: Adapter,
    hint: Option<String>,
    peripheral: Option<Peripheral>,
    write_char: Option<Characteristic>,
    notes: Arc<Mutex<Vec<Vec<u8>>>>,
    notif_task: Option<tokio::task::JoinHandle<()>>,
    chunk_size: usize,
    delay: Duration,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

impl Ble {
    pub async fn new(hint: Option<String>) -> Result<Self> {
        let manager = Manager::new().await?;
        let adapters = manager.adapters().await?;
        let adapter = adapters
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no Bluetooth adapter found"))?;
        Ok(Self {
            adapter,
            hint,
            peripheral: None,
            write_char: None,
            notes: Arc::new(Mutex::new(Vec::new())),
            notif_task: None,
            chunk_size: 128,
            delay: Duration::from_millis(8),
        })
    }

    pub fn is_connected(&self) -> bool {
        self.peripheral.is_some() && self.write_char.is_some()
    }

    fn likely(name: &str, services: &[Uuid]) -> bool {
        let n = name.to_lowercase();
        services.iter().any(|u| *u == SVC_AE30 || *u == SVC_AF30)
            || NAME_HINTS.iter().any(|h| n.contains(h))
    }

    /// Scan and return discovered devices (likely-printers flagged).
    pub async fn scan(&self, secs: u64) -> Result<Vec<DeviceInfo>> {
        self.adapter.start_scan(ScanFilter::default()).await?;
        tokio::time::sleep(Duration::from_secs(secs)).await;
        let peers = self.adapter.peripherals().await?;
        let _ = self.adapter.stop_scan().await;
        let mut out = Vec::new();
        for p in peers {
            if let Ok(Some(props)) = p.properties().await {
                let name = props.local_name.unwrap_or_default();
                let services: Vec<Uuid> = props.services.clone();
                out.push(DeviceInfo {
                    likely_printer: Self::likely(&name, &services),
                    name: if name.is_empty() { "(unknown)".into() } else { name },
                    id: p.id().to_string(),
                    rssi: props.rssi,
                });
            }
        }
        out.sort_by(|a, b| {
            b.likely_printer
                .cmp(&a.likely_printer)
                .then(b.rssi.unwrap_or(-999).cmp(&a.rssi.unwrap_or(-999)))
        });
        Ok(out)
    }

    async fn find(&self) -> Result<Peripheral> {
        self.adapter.start_scan(ScanFilter::default()).await?;
        let hint = self.hint.as_ref().map(|h| h.to_lowercase());
        let mut found = None;
        for _ in 0..24 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            for p in self.adapter.peripherals().await? {
                let Ok(Some(props)) = p.properties().await else { continue };
                let name = props.local_name.unwrap_or_default();
                let matches = match &hint {
                    Some(h) => {
                        name.to_lowercase().contains(h) || p.id().to_string().to_lowercase().contains(h)
                    }
                    None => Self::likely(&name, &props.services),
                };
                if matches {
                    found = Some(p);
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        let _ = self.adapter.stop_scan().await;
        found.ok_or_else(|| anyhow!("printer not found (hint: {:?})", self.hint))
    }

    async fn cleanup(&mut self) {
        if let Some(h) = self.notif_task.take() {
            h.abort();
        }
        if let Some(p) = self.peripheral.take() {
            let _ = tokio::time::timeout(Duration::from_secs(3), p.disconnect()).await;
        }
        self.write_char = None;
    }

    /// Ensure connected, (re)scanning and connecting with retry as needed.
    pub async fn ensure_connected(&mut self) -> Result<()> {
        // Reuse the live connection only if it is verifiably still up.
        if self.write_char.is_some() {
            if let Some(p) = &self.peripheral {
                if let Ok(Ok(true)) =
                    tokio::time::timeout(Duration::from_secs(2), p.is_connected()).await
                {
                    return Ok(());
                }
            }
        }
        // Otherwise drop any stale handle and reconnect fresh.
        self.cleanup().await;
        let mut last_err = anyhow!("unknown");
        for attempt in 1..=3 {
            match self.connect_once().await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::warn!("connect attempt {attempt}/3 failed: {e}");
                    last_err = e;
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            }
        }
        Err(last_err)
    }

    async fn connect_once(&mut self) -> Result<()> {
        let p = self.find().await?;
        p.connect().await?;
        p.discover_services().await?;
        let chars = p.characteristics();
        let write_char = chars
            .iter()
            .find(|c| c.uuid == CHAR_AE01)
            .cloned()
            .ok_or_else(|| anyhow!("characteristic 0xAE01 not found"))?;

        if let Some(notify) = chars.iter().find(|c| c.uuid == CHAR_AE02).cloned() {
            if p.subscribe(&notify).await.is_ok() {
                if let Some(h) = self.notif_task.take() {
                    h.abort();
                }
                let notes = self.notes.clone();
                let pc = p.clone();
                let handle = tokio::spawn(async move {
                    if let Ok(mut stream) = pc.notifications().await {
                        while let Some(v) = stream.next().await {
                            notes.lock().unwrap().push(v.value);
                        }
                    }
                });
                self.notif_task = Some(handle);
            }
        }
        let name = p
            .properties()
            .await
            .ok()
            .flatten()
            .and_then(|pr| pr.local_name)
            .unwrap_or_else(|| "printer".into());
        tracing::info!("connected to {name}");
        self.peripheral = Some(p);
        self.write_char = Some(write_char);
        Ok(())
    }

    pub async fn disconnect(&mut self) -> Result<()> {
        self.cleanup().await;
        Ok(())
    }

    /// Stream a raw command buffer to 0xAE01 in chunks.
    pub async fn send(&mut self, data: &[u8]) -> Result<()> {
        self.ensure_connected().await?;
        let p = self.peripheral.as_ref().unwrap();
        let w = self.write_char.as_ref().unwrap();
        for chunk in data.chunks(self.chunk_size) {
            let write = p.write(w, chunk, WriteType::WithoutResponse);
            match tokio::time::timeout(Duration::from_secs(5), write).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    self.cleanup().await;
                    return Err(anyhow!("write failed: {e}"));
                }
                Err(_) => {
                    self.cleanup().await;
                    return Err(anyhow!("write timed out"));
                }
            }
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
        }
        Ok(())
    }

    pub async fn query_status(&mut self) -> Result<Status> {
        self.ensure_connected().await?;
        self.notes.lock().unwrap().clear();
        self.send(&protocol::get_device_info()).await?;
        self.send(&protocol::get_device_state()).await?;
        tokio::time::sleep(Duration::from_millis(1300)).await;

        let mut ae10_raw = None;
        if let Some(p) = &self.peripheral {
            if let Some(c) = p.characteristics().iter().find(|c| c.uuid == CHAR_AE10) {
                if let Ok(v) = p.read(c).await {
                    ae10_raw = Some(hex(&v));
                }
            }
        }

        let notes = self.notes.lock().unwrap().clone();
        let blob: Vec<u8> = notes.iter().flatten().copied().collect();
        let mut status = Status {
            firmware: None,
            state_raw: None,
            ae10_raw,
            notifications: notes.iter().map(|n| hex(n)).collect(),
            battery: None,
        };
        for (cmd, payload) in protocol::parse_frames(&blob) {
            if cmd == protocol::cmd::GET_DEV_INFO {
                let ascii: String = payload
                    .iter()
                    .filter(|&&b| (32..127).contains(&b))
                    .map(|&b| b as char)
                    .collect();
                let t = ascii.trim().to_string();
                if !t.is_empty() {
                    status.firmware = Some(t);
                }
            } else if cmd == protocol::cmd::GET_DEV_STATE {
                status.state_raw = Some(hex(&payload));
            }
        }
        Ok(status)
    }
}
