# thermorinterd

A cross-platform **print service daemon** for BLE "cat" thermal printers
(the `0xAE30` protocol: GB01/GT01/YY and compatibles, e.g. `TD-11308`).

A single Rust binary that owns the Bluetooth connection and exposes the printer
over a simple HTTP API + a job queue with automatic reconnect/retry. Print from
anything — `curl`, your phone, another app, a webhook, home automation.

- **Cross-platform BLE** via `btleplug` (macOS CoreBluetooth · Linux BlueZ · Windows WinRT)
- **HTTP API**: text, image, QR, raw, feed, status, scan
- **Job queue** with connection keepalive + retry/backoff (handles the printer's
  flaky wake-from-sleep behavior)
- **On-device rendering**: text (system fonts), images (scale-to-384 + Floyd–Steinberg
  dithering), QR codes — same pipeline verified against the Python/TS implementations
- No runtime dependencies — one binary you can drop on a Raspberry Pi

## Build & run

```bash
cargo build --release          # target/release/thermorinterd
./target/release/thermorinterd serve --addr 127.0.0.1:9100
# auto-detects a likely printer; or pin one:
./target/release/thermorinterd serve --device TD-11308     # name substring or id
```

Subcommands:

```bash
thermorinterd scan                 # list nearby BLE devices (★ = likely printer)
thermorinterd status --device TD-  # firmware / state
thermorinterd serve [--addr H:P] [--device NAME|ID] [--font PATH] [--energy N] [--feed N]
```

## HTTP API

| Method | Path | Body | Description |
|---|---|---|---|
| GET  | `/` | — | built-in **web UI** (print text/QR/image/PDF, status) |
| GET  | `/health` | — | liveness (`ok`) |
| GET  | `/status` | — | firmware + raw state (JSON) |
| GET  | `/scan?secs=6` | — | discovered devices (JSON) |
| POST | `/print/text` | JSON | `{ text, font_size?, align?, energy?, feed? }` |
| POST | `/print/qr` | JSON | `{ data, caption?, energy?, feed? }` |
| POST | `/print/image?dither=1&invert=0&energy=&feed=` | raw image bytes | png/jpg/… |
| POST | `/print/pdf?dither=1&invert=0&dpi=200&energy=&feed=` | raw PDF bytes | needs `pdftoppm`/`mutool`/`gs` |
| POST | `/print/rows?energy=&feed=` | binary, multiple of 48 | packed 384px rows (used by the CUPS backend) |
| POST | `/print/raw` | hex text or binary | a raw command stream |
| POST | `/feed` | JSON | `{ lines }` |

Boolean query params accept `1/0`, `true/false`, `yes/no`, `on/off`.
Open `http://127.0.0.1:9100/` in a browser for the built-in UI.

### Examples

```bash
curl -X POST localhost:9100/print/text \
  -H 'content-type: application/json' \
  -d '{"text":"Hello!","align":"center","font_size":30}'

curl -X POST localhost:9100/print/qr \
  -H 'content-type: application/json' \
  -d '{"data":"https://example.com","caption":"scan me"}'

curl -X POST "localhost:9100/print/image?dither=1" \
  --data-binary @photo.png

curl -X POST localhost:9100/feed -H 'content-type: application/json' -d '{"lines":60}'

curl localhost:9100/status
```

## Run as a service

One script installs the binary and registers the OS service (systemd on Linux,
launchd LaunchAgent on macOS):

```bash
./contrib/install.sh --device TD-11308 --addr 0.0.0.0:9100
```

Unit/plist templates live in `contrib/systemd/` and `contrib/launchd/`.

## Virtual printer (CUPS) — print from any app

On a CUPS host (Linux/macOS) you can register the printer so it appears in the
system **Print…** dialog. CUPS converts each job to PDF (via `thermorinter.ppd`)
and hands it to the `thermorinter` CUPS backend, which forwards it to a running
`thermorinterd` at `/print/pdf` — reusing the daemon's renderer, no raster
parsing required.

```bash
thermorinterd serve --addr 127.0.0.1:9100 &        # daemon must be running
sudo ./contrib/cups/install-cups.sh --host 127.0.0.1:9100
lp -d thermorinter document.pdf                    # or Print… from any app
```

Files: `contrib/cups/thermorinter` (backend), `thermorinter.ppd`,
`install-cups.sh`. Best hosted on **Linux/Raspberry Pi** (no BLE permission
friction). On macOS the daemon still needs the Bluetooth permission grant.

## Platform notes

- **Linux / Raspberry Pi**: ideal deployment — no permission friction; also the
  home for the upcoming **CUPS virtual-printer** backend (print from any app).
- **macOS**: the process needs the system Bluetooth permission (granted to the
  terminal/app that launches it), same as any CoreBluetooth app.
- **Windows**: HTTP service works; a true system printer would need a driver.

## Battery

`/status` returns firmware reliably. The classic `0xAE30` firmware does **not**
expose battery/temperature (state is constant; `0xAE10` reads zeros), so
`battery` is `null` there. The field is kept for models that do report it.

## Roadmap

- WebSocket job events for the web UI
- Native PDF rendering (pdfium) to drop the external-tool dependency
- Prometheus metrics; multi-printer support

## License

MIT
