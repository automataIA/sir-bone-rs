use ratatui::{buffer::Buffer, layout::Rect, style::Color, widgets::Widget};

// ── braille animation ─────────────────────────────────────────────────────────

pub type BrailleCell = Option<(char, u8, u8, u8)>;
pub type BrailleFrame = Vec<Vec<BrailleCell>>;

pub struct BoarAnim {
    pub frames: Vec<BrailleFrame>,
    pub frames_flipped: Vec<BrailleFrame>,
    pub w: u16,
    pub h: u16,
    pub tick: u64,
    pub ticks_per_frame: u64,
    pub x: i16,
    pub dir: i16,
}

impl BoarAnim {
    pub fn new_mock(w: u16, h: u16) -> Self {
        let frames = Self::build_mock(w, h);
        let frames_flipped = Self::flip_frames(&frames);
        Self {
            frames,
            frames_flipped,
            w,
            h,
            tick: 0,
            ticks_per_frame: 3,
            x: -(w as i16),
            dir: 1,
        }
    }

    pub fn build_mock(w: u16, h: u16) -> Vec<BrailleFrame> {
        let bit_table: &[u8] = &[0x01, 0x03, 0x07, 0x0F, 0x1F, 0x3F, 0x7F, 0xFF];
        (0..8_usize)
            .map(|f| {
                (0..h as usize)
                    .map(|row| {
                        (0..w as usize)
                            .map(|col| {
                                if (col + row * 3 + f) % 4 != 0 {
                                    return None;
                                }
                                let bits = u32::from(bit_table[(col + row + f) % 8]);
                                let ch = char::from_u32(0x2800 | bits).unwrap_or('⣿');
                                let r = 200_u8.saturating_sub((row as u8).saturating_mul(6));
                                Some((ch, r, 80_u8, 160_u8))
                            })
                            .collect()
                    })
                    .collect()
            })
            .collect()
    }

    /// Parse a BRLF blob. v1 = 4 B/cell (per-cell RGB); v2 = 1 B/cell + a 3 B
    /// global color in the header (monochrome, 4× smaller).
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 13 || &data[0..4] != b"BRLF" {
            return None;
        }
        let version = data[4];
        let w = u16::from_le_bytes([data[5], data[6]]);
        let h = u16::from_le_bytes([data[7], data[8]]);
        let n_frames = u32::from_le_bytes([data[9], data[10], data[11], data[12]]) as usize;
        // Zero frames (e.g. a truncated/corrupt header) would leave `frames`
        // empty and panic the About render on `frames[fi]`. Reject it.
        if n_frames == 0 {
            return None;
        }
        let cell_count = w as usize * h as usize;

        let (data_start, bytes_per_cell, global_color) = match version {
            1 => (13, 4, None),
            2 if data.len() >= 16 => (16, 1, Some((data[13], data[14], data[15]))),
            _ => return None,
        };
        if data.len() < data_start + n_frames * (2 + cell_count * bytes_per_cell) {
            return None;
        }

        let first_delay = u16::from_le_bytes([data[data_start], data[data_start + 1]]) as u64;
        let ticks_per_frame = (first_delay / 16).max(1);

        let mut frames = Vec::with_capacity(n_frames);
        let mut pos = data_start;
        for _ in 0..n_frames {
            pos += 2;
            let mut frame: BrailleFrame = Vec::with_capacity(h as usize);
            for _ in 0..h as usize {
                let mut row: Vec<BrailleCell> = Vec::with_capacity(w as usize);
                for _ in 0..w as usize {
                    let ch_offset = data[pos];
                    let (r, g, b) = global_color
                        .unwrap_or_else(|| (data[pos + 1], data[pos + 2], data[pos + 3]));
                    pos += bytes_per_cell;
                    row.push(if ch_offset == 0 {
                        None
                    } else {
                        char::from_u32(0x2800 | u32::from(ch_offset)).map(|ch| (ch, r, g, b))
                    });
                }
                frame.push(row);
            }
            frames.push(frame);
        }
        let frames_flipped = Self::flip_frames(&frames);
        Some(Self {
            frames,
            frames_flipped,
            w,
            h,
            tick: 0,
            ticks_per_frame,
            x: -(w as i16),
            dir: 1,
        })
    }

    pub fn flip_frames(frames: &[BrailleFrame]) -> Vec<BrailleFrame> {
        frames
            .iter()
            .map(|frame| {
                frame
                    .iter()
                    .map(|row| {
                        row.iter()
                            .cloned()
                            .rev()
                            .map(|cell| cell.map(|(ch, r, g, b)| (Self::flip_braille(ch), r, g, b)))
                            .collect()
                    })
                    .collect()
            })
            .collect()
    }

    pub fn flip_braille(ch: char) -> char {
        let cp = ch as u32;
        if !(0x2800..=0x28FF).contains(&cp) {
            return ch;
        }
        let b = (cp - 0x2800) as u8;
        let f = ((b & 0x01) << 3)
            | ((b & 0x02) << 3)
            | ((b & 0x04) << 3)
            | ((b & 0x08) >> 3)
            | ((b & 0x10) >> 3)
            | ((b & 0x20) >> 3)
            | ((b & 0x40) << 1)
            | ((b & 0x80) >> 1);
        char::from_u32(0x2800 | u32::from(f)).unwrap_or(ch)
    }

    pub fn advance_panel(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }

    pub fn advance_about(&mut self, boar_area_w: u16) {
        self.tick = self.tick.wrapping_add(1);
        if !self.tick.is_multiple_of(self.ticks_per_frame) {
            return;
        }
        self.x += self.dir * 2;
        if self.x > boar_area_w as i16 {
            self.dir = -1;
        } else if self.x < -(self.w as i16) {
            self.dir = 1;
        }
    }

    pub fn frame_idx(&self) -> usize {
        (self.tick / self.ticks_per_frame) as usize % self.frames.len().max(1)
    }

    pub fn current_frames(&self) -> &[BrailleFrame] {
        if self.dir == -1 {
            &self.frames_flipped
        } else {
            &self.frames
        }
    }
}

pub fn load_boar() -> Option<BoarAnim> {
    // Embedded at compile time — keeps the binary self-contained (no runtime asset path).
    BoarAnim::from_bytes(include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/sirbone.bin"
    )))
}

pub struct BrailleWidget<'a> {
    pub frame: &'a BrailleFrame,
    pub x_offset: i16,
}

impl Widget for BrailleWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        for (ri, row) in self.frame.iter().enumerate() {
            let y = area.y + ri as u16;
            if y >= area.y + area.height {
                break;
            }
            for (ci, cell) in row.iter().enumerate() {
                let sx = self.x_offset + ci as i16;
                if sx < 0 {
                    continue;
                }
                let x = area.x.saturating_add(sx.unsigned_abs());
                if x >= area.x + area.width {
                    break;
                }
                if let Some((ch, r, g, b)) = cell {
                    buf[(x, y)].set_char(*ch).set_fg(Color::Rgb(*r, *g, *b));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boar_from_bytes_parses_v1_and_v2() {
        // Common 1×1, 1-frame header for width=1, height=1, n_frames=1, delay=32ms.
        let hdr = |ver: u8| {
            let mut v = b"BRLF".to_vec();
            v.push(ver);
            v.extend_from_slice(&1u16.to_le_bytes()); // w
            v.extend_from_slice(&1u16.to_le_bytes()); // h
            v.extend_from_slice(&1u32.to_le_bytes()); // n_frames
            v
        };

        // v1: per-cell RGB (offset, r, g, b)
        let mut v1 = hdr(1);
        v1.extend_from_slice(&32u16.to_le_bytes()); // delay
        v1.extend_from_slice(&[0x09, 1, 2, 3]); // one cell
        let b1 = BoarAnim::from_bytes(&v1).expect("v1 parses");
        assert_eq!((b1.w, b1.h, b1.ticks_per_frame), (1, 1, 2));
        assert_eq!(b1.frames[0][0][0], Some(('\u{2809}', 1, 2, 3)));

        // v2: 3-byte global color + 1 byte/cell
        let mut v2 = hdr(2);
        v2.extend_from_slice(&[10, 20, 30]); // global color
        v2.extend_from_slice(&32u16.to_le_bytes()); // delay
        v2.push(0xFF); // one cell offset
        let b2 = BoarAnim::from_bytes(&v2).expect("v2 parses");
        assert_eq!((b2.w, b2.h), (1, 1));
        assert_eq!(b2.frames[0][0][0], Some(('\u{28FF}', 10, 20, 30)));

        // Truncated / unknown version rejected.
        assert!(BoarAnim::from_bytes(&v2[..v2.len() - 1]).is_none());
        assert!(BoarAnim::from_bytes(&hdr(9)).is_none());
    }
}
