//! Minimal EXIF (TIFF) reader/writer used to keep a small whitelist of
//! harmless tags when stripping in the default (non `--all`) mode.
//!
//! We never copy an EXIF block through. Instead we read the whitelisted
//! values out of it and serialize a brand-new TIFF structure containing only
//! those values, so nothing else (maker notes, GPS, thumbnails, unknown tags,
//! padding) can leak through.

const TAG_ORIENTATION: u16 = 0x0112;
const TAG_X_RESOLUTION: u16 = 0x011A;
const TAG_Y_RESOLUTION: u16 = 0x011B;
const TAG_RESOLUTION_UNIT: u16 = 0x0128;
const TAG_EXIF_IFD: u16 = 0x8769;
const TAG_COLOR_SPACE: u16 = 0xA001;

const TYPE_SHORT: u16 = 3;
const TYPE_LONG: u16 = 4;
const TYPE_RATIONAL: u16 = 5;

/// The non-identifying tags we preserve. They only affect how the image is
/// displayed, never who/where/when/with-what it was taken.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Kept {
    pub orientation: Option<u16>,
    pub x_resolution: Option<(u32, u32)>,
    pub y_resolution: Option<(u32, u32)>,
    pub resolution_unit: Option<u16>,
    pub color_space: Option<u16>,
}

impl Kept {
    /// Reads the whitelisted tags out of a raw TIFF/EXIF block.
    /// Accepts an optional leading `Exif\0\0` header. Malformed input yields
    /// whatever could be read (possibly nothing) rather than an error, since
    /// the worst case is simply dropping the tag.
    pub fn read(tiff: &[u8]) -> Kept {
        let tiff = tiff.strip_prefix(b"Exif\0\0").unwrap_or(tiff);
        let mut kept = Kept::default();
        let r = match Reader::new(tiff) {
            Some(r) => r,
            None => return kept,
        };
        let ifd0 = match r.u32(4) {
            Some(o) => o as usize,
            None => return kept,
        };
        let mut exif_ifd = None;
        r.each_entry(ifd0, |tag, ty, count, value_at| match (tag, ty, count) {
            (TAG_ORIENTATION, TYPE_SHORT, 1) => {
                kept.orientation = r.u16(value_at).filter(|v| (1..=8).contains(v))
            }
            (TAG_RESOLUTION_UNIT, TYPE_SHORT, 1) => {
                kept.resolution_unit = r.u16(value_at).filter(|v| (1..=3).contains(v))
            }
            (TAG_X_RESOLUTION, TYPE_RATIONAL, 1) => kept.x_resolution = r.rational(value_at),
            (TAG_Y_RESOLUTION, TYPE_RATIONAL, 1) => kept.y_resolution = r.rational(value_at),
            (TAG_EXIF_IFD, TYPE_LONG, 1) => exif_ifd = r.u32(value_at).map(|o| o as usize),
            _ => {}
        });
        if let Some(off) = exif_ifd {
            r.each_entry(off, |tag, ty, count, value_at| {
                if (tag, ty, count) == (TAG_COLOR_SPACE, TYPE_SHORT, 1) {
                    kept.color_space = r
                        .u16(value_at)
                        .filter(|v| *v == 1 || *v == 2 || *v == 0xFFFF);
                }
            });
        }
        kept
    }

    pub fn is_empty(&self) -> bool {
        *self == Kept::default()
    }

    /// Serializes the kept tags as a fresh big-endian TIFF block (no `Exif\0\0` header).
    /// Output is deterministic, so re-stripping an already stripped file is a no-op.
    pub fn to_tiff(&self) -> Vec<u8> {
        enum Val {
            Short(u16),
            ExifIfdPointer,
            Rational(u32, u32),
        }
        let mut ifd0: Vec<(u16, Val)> = Vec::new();
        if let Some(v) = self.orientation {
            ifd0.push((TAG_ORIENTATION, Val::Short(v)));
        }
        if let Some((n, d)) = self.x_resolution {
            ifd0.push((TAG_X_RESOLUTION, Val::Rational(n, d)));
        }
        if let Some((n, d)) = self.y_resolution {
            ifd0.push((TAG_Y_RESOLUTION, Val::Rational(n, d)));
        }
        if let Some(v) = self.resolution_unit {
            ifd0.push((TAG_RESOLUTION_UNIT, Val::Short(v)));
        }
        let has_exif_ifd = self.color_space.is_some();
        if has_exif_ifd {
            ifd0.push((TAG_EXIF_IFD, Val::ExifIfdPointer));
        }

        let ifd_size = |n: usize| 2 + n * 12 + 4;
        let ifd0_off = 8usize;
        let mut data_off = ifd0_off + ifd_size(ifd0.len());
        let rationals = ifd0
            .iter()
            .filter(|(_, v)| matches!(v, Val::Rational(..)))
            .count();
        let exif_ifd_off = data_off + rationals * 8;

        let mut out = b"MM\0\x2A".to_vec();
        out.extend_from_slice(&(ifd0_off as u32).to_be_bytes());
        out.extend_from_slice(&(ifd0.len() as u16).to_be_bytes());
        let mut extra = Vec::new();
        for (tag, val) in &ifd0 {
            out.extend_from_slice(&tag.to_be_bytes());
            match val {
                Val::Short(v) => {
                    out.extend_from_slice(&TYPE_SHORT.to_be_bytes());
                    out.extend_from_slice(&1u32.to_be_bytes());
                    out.extend_from_slice(&v.to_be_bytes());
                    out.extend_from_slice(&[0, 0]);
                }
                Val::ExifIfdPointer => {
                    out.extend_from_slice(&TYPE_LONG.to_be_bytes());
                    out.extend_from_slice(&1u32.to_be_bytes());
                    out.extend_from_slice(&(exif_ifd_off as u32).to_be_bytes());
                }
                Val::Rational(n, d) => {
                    out.extend_from_slice(&TYPE_RATIONAL.to_be_bytes());
                    out.extend_from_slice(&1u32.to_be_bytes());
                    out.extend_from_slice(&(data_off as u32).to_be_bytes());
                    extra.extend_from_slice(&n.to_be_bytes());
                    extra.extend_from_slice(&d.to_be_bytes());
                    data_off += 8;
                }
            }
        }
        out.extend_from_slice(&0u32.to_be_bytes()); // no IFD1 (=> no thumbnail)
        out.extend_from_slice(&extra);

        if let Some(cs) = self.color_space {
            debug_assert_eq!(out.len(), exif_ifd_off);
            out.extend_from_slice(&1u16.to_be_bytes());
            out.extend_from_slice(&TAG_COLOR_SPACE.to_be_bytes());
            out.extend_from_slice(&TYPE_SHORT.to_be_bytes());
            out.extend_from_slice(&1u32.to_be_bytes());
            out.extend_from_slice(&cs.to_be_bytes());
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(&0u32.to_be_bytes());
        }
        out
    }
}

struct Reader<'a> {
    d: &'a [u8],
    le: bool,
}

impl<'a> Reader<'a> {
    fn new(d: &'a [u8]) -> Option<Self> {
        let le = match d.get(0..4)? {
            b"II\x2A\0" => true,
            b"MM\0\x2A" => false,
            _ => return None,
        };
        Some(Reader { d, le })
    }

    fn u16(&self, at: usize) -> Option<u16> {
        let b: [u8; 2] = self.d.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if self.le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    }

    fn u32(&self, at: usize) -> Option<u32> {
        let b: [u8; 4] = self.d.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if self.le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    fn rational(&self, value_at: usize) -> Option<(u32, u32)> {
        let off = self.u32(value_at)? as usize;
        let (n, d) = (self.u32(off)?, self.u32(off.checked_add(4)?)?);
        if d == 0 {
            None
        } else {
            Some((n, d))
        }
    }

    /// Calls `f(tag, type, count, offset_of_value_field)` for each IFD entry.
    fn each_entry(&self, ifd: usize, mut f: impl FnMut(u16, u16, u32, usize)) {
        let n = match self.u16(ifd) {
            Some(n) => n as usize,
            None => return,
        };
        for i in 0..n {
            let e = ifd + 2 + i * 12;
            match (self.u16(e), self.u16(e + 2), self.u32(e + 4)) {
                (Some(tag), Some(ty), Some(count)) if e + 12 <= self.d.len() => {
                    f(tag, ty, count, e + 8)
                }
                _ => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_is_stable() {
        let k = Kept {
            orientation: Some(6),
            x_resolution: Some((72, 1)),
            y_resolution: Some((72, 1)),
            resolution_unit: Some(2),
            color_space: Some(1),
        };
        let t = k.to_tiff();
        assert_eq!(Kept::read(&t), k);
        assert_eq!(Kept::read(&t).to_tiff(), t);
        assert_eq!(Kept::read(&[b"Exif\0\0".as_ref(), &t].concat()), k);
    }

    #[test]
    fn reads_little_endian_and_ignores_other_tags() {
        // II, IFD0 at 8 with 2 entries: Make (ASCII) and Orientation=3.
        let mut t = b"II\x2A\0\x08\0\0\0".to_vec();
        t.extend_from_slice(&2u16.to_le_bytes());
        t.extend_from_slice(&[0x0F, 0x01, 2, 0, 4, 0, 0, 0, b'C', b'a', b'm', 0]);
        t.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 3, 0, 0, 0]);
        t.extend_from_slice(&[0, 0, 0, 0]);
        let k = Kept::read(&t);
        assert_eq!(
            k,
            Kept {
                orientation: Some(3),
                ..Kept::default()
            }
        );
    }

    #[test]
    fn garbage_yields_nothing() {
        assert!(Kept::read(b"not a tiff").is_empty());
        assert!(Kept::read(b"MM\0\x2A\xFF\xFF\xFF\xFF").is_empty());
    }
}
