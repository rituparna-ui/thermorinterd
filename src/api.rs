//! HTTP print service (axum). Renders requests to command streams and hands
//! them to the BLE worker queue.

use std::sync::Arc;

use ab_glyph::FontVec;
use axum::{
    body::Bytes,
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::oneshot;

use crate::protocol::{self, JobOptions, BYTES_PER_ROW};
use crate::render::{self, Align, ImageOptions};
use crate::worker::{Job, Sender};

#[derive(Clone)]
pub struct AppState {
    pub tx: Sender,
    pub font: Arc<Option<FontVec>>,
    pub energy: u16,
    pub feed: u16,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(|| async { Html(include_str!("webui.html")) }))
        .route("/health", get(|| async { "ok" }))
        .route("/status", get(status))
        .route("/scan", get(scan))
        .route("/print/text", post(print_text))
        .route("/print/qr", post(print_qr))
        .route("/print/image", post(print_image))
        .route("/print/pdf", post(print_pdf))
        .route("/print/rows", post(print_rows))
        .route("/print/raw", post(print_raw))
        .route("/feed", post(feed))
        .with_state(state)
}

type ApiError = (StatusCode, Json<serde_json::Value>);

fn err(code: StatusCode, msg: impl ToString) -> ApiError {
    (code, Json(json!({ "error": msg.to_string() })))
}

async fn submit_print(tx: &Sender, data: Vec<u8>) -> Result<(), ApiError> {
    let (rtx, rrx) = oneshot::channel();
    tx.send(Job::Print { data, resp: rtx })
        .await
        .map_err(|e| err(StatusCode::SERVICE_UNAVAILABLE, e))?;
    rrx.await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .map_err(|e| err(StatusCode::BAD_GATEWAY, e))
}

fn ok() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}

fn parse_align(s: Option<&str>) -> Align {
    match s.unwrap_or("left") {
        "center" => Align::Center,
        "right" => Align::Right,
        _ => Align::Left,
    }
}

async fn status(State(st): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let (rtx, rrx) = oneshot::channel();
    st.tx
        .send(Job::Status { resp: rtx })
        .await
        .map_err(|e| err(StatusCode::SERVICE_UNAVAILABLE, e))?;
    let s = rrx
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;
    Ok(Json(s))
}

#[derive(Deserialize)]
struct ScanQuery {
    secs: Option<u64>,
}

async fn scan(
    State(st): State<AppState>,
    Query(q): Query<ScanQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let (rtx, rrx) = oneshot::channel();
    st.tx
        .send(Job::Scan { secs: q.secs.unwrap_or(6), resp: rtx })
        .await
        .map_err(|e| err(StatusCode::SERVICE_UNAVAILABLE, e))?;
    let d = rrx
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;
    Ok(Json(d))
}

fn job_opts(st: &AppState, energy: Option<u16>, feed: Option<u16>) -> JobOptions {
    JobOptions {
        energy: energy.unwrap_or(st.energy),
        feed_lines: feed.unwrap_or(st.feed),
        ..Default::default()
    }
}

#[derive(Deserialize)]
struct TextReq {
    text: String,
    #[serde(default)]
    font_size: Option<f32>,
    #[serde(default)]
    align: Option<String>,
    #[serde(default)]
    energy: Option<u16>,
    #[serde(default)]
    feed: Option<u16>,
}

async fn print_text(
    State(st): State<AppState>,
    Json(req): Json<TextReq>,
) -> Result<impl IntoResponse, ApiError> {
    let font = st
        .font
        .as_ref()
        .as_ref()
        .ok_or_else(|| err(StatusCode::PRECONDITION_FAILED, "no font available; start with --font PATH"))?;
    let rows = render::text_to_rows(font, &req.text, req.font_size.unwrap_or(28.0), parse_align(req.align.as_deref()))
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let data = protocol::build_job(&rows, &job_opts(&st, req.energy, req.feed));
    submit_print(&st.tx, data).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct QrReq {
    data: String,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    energy: Option<u16>,
    #[serde(default)]
    feed: Option<u16>,
}

async fn print_qr(
    State(st): State<AppState>,
    Json(req): Json<QrReq>,
) -> Result<impl IntoResponse, ApiError> {
    let font = st
        .font
        .as_ref()
        .as_ref()
        .ok_or_else(|| err(StatusCode::PRECONDITION_FAILED, "no font available; start with --font PATH"))?;
    let rows = render::qr_to_rows(font, &req.data, req.caption.as_deref())
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    let data = protocol::build_job(&rows, &job_opts(&st, req.energy, req.feed));
    submit_print(&st.tx, data).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct ImageQuery {
    #[serde(default = "default_true", deserialize_with = "de_bool")]
    dither: bool,
    #[serde(default, deserialize_with = "de_bool")]
    invert: bool,
    #[serde(default)]
    energy: Option<u16>,
    #[serde(default)]
    feed: Option<u16>,
}

fn default_true() -> bool {
    true
}

/// Accept 1/0, true/false, yes/no, on/off for boolean query params.
fn de_bool<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    use serde::Deserialize;
    let s = String::deserialize(d)?;
    Ok(matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

/// POST raw image bytes (png/jpg/…) in the body; options via query string.
async fn print_image(
    State(st): State<AppState>,
    Query(q): Query<ImageQuery>,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    if body.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "empty body; POST image bytes"));
    }
    let opts = ImageOptions { dither: q.dither, invert: q.invert, ..Default::default() };
    let rows = render::image_to_rows(&body, &opts).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    let data = protocol::build_job(&rows, &job_opts(&st, q.energy, q.feed));
    submit_print(&st.tx, data).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct PdfQuery {
    #[serde(default = "default_true", deserialize_with = "de_bool")]
    dither: bool,
    #[serde(default, deserialize_with = "de_bool")]
    invert: bool,
    #[serde(default = "default_dpi")]
    dpi: u32,
    #[serde(default)]
    energy: Option<u16>,
    #[serde(default)]
    feed: Option<u16>,
}

fn default_dpi() -> u32 {
    200
}

/// POST raw PDF bytes; rasterized via an external tool (pdftoppm/mutool/gs).
async fn print_pdf(
    State(st): State<AppState>,
    Query(q): Query<PdfQuery>,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    if body.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "empty body; POST pdf bytes"));
    }
    let opts = ImageOptions { dither: q.dither, invert: q.invert, ..Default::default() };
    let rows = render::pdf_to_rows(&body, &opts, q.dpi).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    let data = protocol::build_job(&rows, &job_opts(&st, q.energy, q.feed));
    submit_print(&st.tx, data).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct RowsQuery {
    #[serde(default)]
    energy: Option<u16>,
    #[serde(default)]
    feed: Option<u16>,
}

/// POST packed 48-byte rows (concatenated binary); daemon wraps them in a job.
/// Used by the CUPS backend, which converts PWG raster lines to rows.
async fn print_rows(
    State(st): State<AppState>,
    Query(q): Query<RowsQuery>,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    if body.is_empty() || body.len() % BYTES_PER_ROW != 0 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("body length must be a non-zero multiple of {BYTES_PER_ROW}"),
        ));
    }
    let rows: Vec<[u8; BYTES_PER_ROW]> = body
        .chunks_exact(BYTES_PER_ROW)
        .map(|c| {
            let mut r = [0u8; BYTES_PER_ROW];
            r.copy_from_slice(c);
            r
        })
        .collect();
    let data = protocol::build_job(&rows, &job_opts(&st, q.energy, q.feed));
    submit_print(&st.tx, data).await?;
    Ok(ok())
}

/// POST a raw command stream: body is either hex text or binary bytes.
async fn print_raw(State(st): State<AppState>, body: Bytes) -> Result<impl IntoResponse, ApiError> {
    let data = decode_maybe_hex(&body);
    submit_print(&st.tx, data).await?;
    Ok(ok())
}

fn decode_maybe_hex(body: &[u8]) -> Vec<u8> {
    if let Ok(s) = std::str::from_utf8(body) {
        let trimmed: String = s.split_whitespace().collect();
        if !trimmed.is_empty() && trimmed.len() % 2 == 0 && trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
            if let Ok(bytes) = hex::decode(&trimmed) {
                return bytes;
            }
        }
    }
    body.to_vec()
}

#[derive(Deserialize)]
struct FeedReq {
    #[serde(default = "default_feed")]
    lines: u16,
}

fn default_feed() -> u16 {
    80
}

async fn feed(
    State(st): State<AppState>,
    Json(req): Json<FeedReq>,
) -> Result<impl IntoResponse, ApiError> {
    submit_print(&st.tx, protocol::feed_paper(req.lines)).await?;
    Ok(ok())
}
