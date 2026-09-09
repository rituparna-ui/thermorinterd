//! Cat thermal printer protocol (service 0xAE30 / characteristic 0xAE01).
//!
//! Framing:  51 78 <cmd> 00 <len_lo> <len_hi> <payload...> <crc8> ff
//! Checksum: CRC-8, polynomial 0x07, init 0x00, no reflection, over payload.
//! Bitmap row (cmd 0xA2): 48 bytes = 384 px, LSB-first (bit 0x01 = leftmost),
//!   a set bit = a black dot.
//!
//! Verified byte-for-byte against real captures.

pub const PRINT_WIDTH: usize = 384;
pub const BYTES_PER_ROW: usize = PRINT_WIDTH / 8; // 48

pub mod cmd {
    pub const RETRACT_PAPER: u8 = 0xA0;
    pub const FEED_PAPER: u8 = 0xA1;
    pub const DRAW_BITMAP: u8 = 0xA2;
    pub const GET_DEV_STATE: u8 = 0xA3;
    pub const SET_QUALITY: u8 = 0xA4;
    pub const CONTROL_LATTICE: u8 = 0xA6;
    pub const GET_DEV_INFO: u8 = 0xA8;
    pub const SET_ENERGY: u8 = 0xAF;
    pub const SET_SPEED: u8 = 0xBD;
    pub const DRAWING_MODE: u8 = 0xBE;
}

pub mod mode {
    pub const IMAGE: u8 = 0x00;
    pub const TEXT: u8 = 0x01;
}

const LATTICE_START: [u8; 11] =
    [0xAA, 0x55, 0x17, 0x38, 0x44, 0x5F, 0x5F, 0x5F, 0x44, 0x38, 0x2C];
const LATTICE_END: [u8; 11] =
    [0xAA, 0x55, 0x17, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x17];

/// CRC-8 lookup table for polynomial 0x07 (computed once).
fn crc_table() -> &'static [u8; 256] {
    use std::sync::OnceLock;
    static TABLE: OnceLock<[u8; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u8; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u8;
            for _ in 0..8 {
                c = if c & 0x80 != 0 {
                    (c << 1) ^ 0x07
                } else {
                    c << 1
                };
            }
            *slot = c;
        }
        t
    })
}

pub fn crc8(data: &[u8]) -> u8 {
    let table = crc_table();
    let mut crc: u8 = 0;
    for &b in data {
        crc = table[(crc ^ b) as usize];
    }
    crc
}

/// Frame a single command: 51 78 cmd 00 len_lo len_hi payload crc ff.
pub fn command(cmd: u8, payload: &[u8]) -> Vec<u8> {
    let len = payload.len();
    let mut out = Vec::with_capacity(6 + len + 2);
    out.extend_from_slice(&[0x51, 0x78, cmd, 0x00, (len & 0xFF) as u8, ((len >> 8) & 0xFF) as u8]);
    out.extend_from_slice(payload);
    out.push(crc8(payload));
    out.push(0xFF);
    out
}

pub fn get_device_state() -> Vec<u8> {
    command(cmd::GET_DEV_STATE, &[0x00])
}
pub fn get_device_info() -> Vec<u8> {
    command(cmd::GET_DEV_INFO, &[0x00])
}
pub fn set_quality(level: u8) -> Vec<u8> {
    command(cmd::SET_QUALITY, &[level])
}
pub fn lattice_start() -> Vec<u8> {
    command(cmd::CONTROL_LATTICE, &LATTICE_START)
}
pub fn lattice_end() -> Vec<u8> {
    command(cmd::CONTROL_LATTICE, &LATTICE_END)
}
pub fn set_speed(speed: u8) -> Vec<u8> {
    command(cmd::SET_SPEED, &[speed])
}
pub fn drawing_mode(m: u8) -> Vec<u8> {
    command(cmd::DRAWING_MODE, &[m])
}
pub fn set_energy(energy: u16) -> Vec<u8> {
    command(cmd::SET_ENERGY, &[(energy & 0xFF) as u8, (energy >> 8) as u8])
}
pub fn feed_paper(lines: u16) -> Vec<u8> {
    command(cmd::FEED_PAPER, &[(lines & 0xFF) as u8, (lines >> 8) as u8])
}

/// Pack a 384-length row of booleans (true = black) into 48 bytes, LSB-first.
pub fn encode_row(row_bits: &[bool]) -> [u8; BYTES_PER_ROW] {
    let mut out = [0u8; BYTES_PER_ROW];
    for x in 0..PRINT_WIDTH.min(row_bits.len()) {
        if row_bits[x] {
            out[x >> 3] |= 1 << (x & 7);
        }
    }
    out
}

pub fn print_row(row_bits: &[bool]) -> Vec<u8> {
    command(cmd::DRAW_BITMAP, &encode_row(row_bits))
}

/// Options controlling a full print job.
#[derive(Clone, Copy, Debug)]
pub struct JobOptions {
    pub energy: u16,
    pub feed_lines: u16,
    pub quality: u8,
    pub mode: u8,
}

impl Default for JobOptions {
    fn default() -> Self {
        Self { energy: 0x3000, feed_lines: 80, quality: 0x33, mode: mode::IMAGE }
    }
}

/// Build the full command stream to print packed 48-byte rows.
pub fn build_job(rows: &[[u8; BYTES_PER_ROW]], opts: &JobOptions) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(get_device_state());
    out.extend(set_quality(opts.quality));
    out.extend(lattice_start());
    out.extend(set_energy(opts.energy));
    out.extend(drawing_mode(opts.mode));
    out.extend(set_speed(0x1E));
    for r in rows {
        out.extend(command(cmd::DRAW_BITMAP, r));
    }
    out.extend(lattice_end());
    if opts.feed_lines > 0 {
        out.extend(feed_paper(opts.feed_lines));
    }
    out.extend(get_device_state());
    out
}

/// Parse a byte stream into (cmd, payload) frames (used for notifications).
pub fn parse_frames(data: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut frames = Vec::new();
    let n = data.len();
    let mut i = 0usize;
    while i + 6 <= n {
        if data[i] != 0x51 || data[i + 1] != 0x78 {
            i += 1;
            continue;
        }
        let c = data[i + 2];
        let len = (data[i + 4] as usize) | ((data[i + 5] as usize) << 8);
        let end = i + 6 + len;
        if end > n {
            break;
        }
        frames.push((c, data[i + 6..end].to_vec()));
        i = end + 2;
    }
    frames
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{:02x}", x)).collect()
    }

    #[test]
    fn crc_vectors() {
        assert_eq!(crc8(&[0x33]), 0x99);
        assert_eq!(crc8(&[0xE0, 0x2E]), 0x89);
    }

    #[test]
    fn framing_vectors() {
        assert_eq!(hex(&get_device_state()), "5178a30001000000ff");
        assert_eq!(hex(&set_quality(0x33)), "5178a40001003399ff");
        assert_eq!(hex(&set_energy(0x2EE0)), "5178af000200e02e89ff");
        assert_eq!(hex(&lattice_start()), "5178a6000b00aa551738445f5f5f44382ca1ff");
        assert_eq!(hex(&lattice_end()), "5178a6000b00aa5517000000000000001711ff");
        assert_eq!(hex(&feed_paper(0x30)), "5178a10002003000f9ff");
        assert_eq!(hex(&drawing_mode(0)), "5178be0001000000ff");
    }

    #[test]
    fn row_lsb_first() {
        let mut bits = vec![false; PRINT_WIDTH];
        bits[0] = true;
        bits[7] = true;
        bits[8] = true;
        let row = encode_row(&bits);
        assert_eq!(row.len(), BYTES_PER_ROW);
        assert_eq!(row[0], 0x81);
        assert_eq!(row[1], 0x01);
    }

    #[test]
    fn parse_roundtrip() {
        let f = parse_frames(&get_device_state());
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].0, cmd::GET_DEV_STATE);
        assert_eq!(f[0].1, vec![0x00]);
    }
}
