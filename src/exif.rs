//! Minimal EXIF (TIFF) reader/writer. Used to keep a small whitelist of
//! harmless tags when stripping in the default (non `--all`) mode, and to
//! list every tag for `metarm -i`.
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

// ------------------------------------------------------------- listing ----

const TAG_GPS_IFD: u16 = 0x8825;
const TAG_INTEROP_IFD: u16 = 0xA005;
const TAG_THUMBNAIL_LENGTH: u16 = 0x0202;

/// Which IFD a tag lives in. Tag numbers are only unique per IFD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ifd {
    Main,
    Exif,
    Gps,
    Interop,
    Thumbnail,
}

/// One EXIF entry, decoded for display.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub ifd: Ifd,
    pub tag: u16,
    pub name: String,
    pub value: String,
    /// Whether the default (non `--all`) strip keeps this entry.
    pub kept_by_default: bool,
}

/// Decodes every entry of an EXIF block for display. Pointer tags (to the
/// EXIF/GPS/interop sub-IFDs) are followed, not listed. The thumbnail IFD is
/// summarised as a single entry.
pub fn list(tiff: &[u8]) -> Vec<Entry> {
    let tiff = tiff.strip_prefix(b"Exif\0\0").unwrap_or(tiff);
    let r = match Reader::new(tiff) {
        Some(r) => r,
        None => return Vec::new(),
    };
    let kept = Kept::read(tiff);
    let mut out = Vec::new();
    let mut visited = Vec::new();
    if let Some(ifd0) = r.u32(4) {
        r.list_ifd(ifd0 as usize, Ifd::Main, &kept, &mut out, &mut visited);
        // IFD1 (the thumbnail) follows IFD0.
        if let Some(n) = r.u16(ifd0 as usize) {
            if let Some(ifd1) = r.u32(ifd0 as usize + 2 + n as usize * 12) {
                if ifd1 != 0 {
                    let mut thumb = Vec::new();
                    r.list_ifd(
                        ifd1 as usize,
                        Ifd::Thumbnail,
                        &kept,
                        &mut thumb,
                        &mut visited,
                    );
                    let size = thumb
                        .iter()
                        .find(|e| e.tag == TAG_THUMBNAIL_LENGTH)
                        .map(|e| format!("{} bytes", e.value))
                        .unwrap_or_else(|| "present".into());
                    out.push(Entry {
                        ifd: Ifd::Thumbnail,
                        tag: 0,
                        name: "Thumbnail".into(),
                        value: format!("embedded preview image, {}", size),
                        kept_by_default: false,
                    });
                }
            }
        }
    }
    add_gps_position(&mut out);
    out
}

impl Reader<'_> {
    fn list_ifd(
        &self,
        ifd: usize,
        kind: Ifd,
        kept: &Kept,
        out: &mut Vec<Entry>,
        visited: &mut Vec<usize>,
    ) {
        // Guard against IFD pointer loops in corrupt files.
        if visited.contains(&ifd) || visited.len() > 16 {
            return;
        }
        visited.push(ifd);
        let mut subs = Vec::new();
        self.each_entry(ifd, |tag, ty, count, value_at| {
            let sub = match (kind, tag) {
                (Ifd::Main, TAG_EXIF_IFD) => Some(Ifd::Exif),
                (Ifd::Main, TAG_GPS_IFD) => Some(Ifd::Gps),
                (Ifd::Exif, TAG_INTEROP_IFD) => Some(Ifd::Interop),
                _ => None,
            };
            if let Some(sub) = sub {
                if let Some(off) = self.u32(value_at) {
                    subs.push((off as usize, sub));
                }
                return;
            }
            let kept_by_default = match (kind, tag) {
                (Ifd::Main, TAG_ORIENTATION) => kept.orientation.is_some(),
                (Ifd::Main, TAG_X_RESOLUTION) => kept.x_resolution.is_some(),
                (Ifd::Main, TAG_Y_RESOLUTION) => kept.y_resolution.is_some(),
                (Ifd::Main, TAG_RESOLUTION_UNIT) => kept.resolution_unit.is_some(),
                (Ifd::Exif, TAG_COLOR_SPACE) => kept.color_space.is_some(),
                _ => false,
            };
            out.push(Entry {
                ifd: kind,
                tag,
                name: tag_name(kind, tag)
                    .map(String::from)
                    .unwrap_or_else(|| format!("Tag 0x{:04X}", tag)),
                value: self.format_value(kind, tag, ty, count, value_at),
                kept_by_default,
            });
        });
        for (off, sub) in subs {
            self.list_ifd(off, sub, kept, out, visited);
        }
    }

    /// Returns the raw bytes of an entry's value (inline or at an offset).
    fn value_bytes(&self, ty: u16, count: u32, value_at: usize) -> Option<&[u8]> {
        let size = type_size(ty)?.checked_mul(count as usize)?;
        let start = if size <= 4 {
            value_at
        } else {
            self.u32(value_at)? as usize
        };
        self.d.get(start..start.checked_add(size)?)
    }

    fn format_value(&self, ifd: Ifd, tag: u16, ty: u16, count: u32, value_at: usize) -> String {
        let bytes = match self.value_bytes(ty, count, value_at) {
            Some(b) => b,
            None => return "<invalid>".into(),
        };
        let n = count as usize;
        let at = |i: usize| bytes.as_ptr() as usize - self.d.as_ptr() as usize + i;

        // Tags whose raw value needs interpretation.
        match (ifd, tag, ty) {
            (Ifd::Main, 0x9C9B..=0x9C9F, 1) => return utf16le(bytes), // XP* (Windows)
            (Ifd::Exif, 0x9286, 7) => return user_comment(bytes),
            (Ifd::Exif, 0x9000, 7) | (Ifd::Exif, 0xA000, 7) | (Ifd::Interop, 0x0002, 7) => {
                return String::from_utf8_lossy(bytes).into_owned()
            }
            (Ifd::Gps, 2 | 4 | 7, 5) if n == 3 => {
                let v: Vec<f64> = (0..3).filter_map(|i| self.urational(at(i * 8))).collect();
                if v.len() == 3 {
                    return if tag == 7 {
                        format!("{:02}:{:02}:{:05.2} UTC", v[0], v[1], v[2])
                    } else {
                        format!("{}\u{b0} {}' {:.2}\"", v[0], v[1], v[2])
                    };
                }
            }
            _ => {}
        }

        let raw = match ty {
            2 => {
                let s = bytes.split(|&b| b == 0).next().unwrap_or(&[]);
                String::from_utf8_lossy(s).trim_end().to_string()
            }
            1 | 6 | 7 if n > 8 => format!("<{} bytes>", n),
            1 | 7 => join(bytes.iter().map(|b| b.to_string())),
            6 => join(bytes.iter().map(|&b| (b as i8).to_string())),
            3 => join((0..n.min(16)).filter_map(|i| self.u16(at(i * 2)).map(|v| v.to_string()))),
            8 => join(
                (0..n.min(16)).filter_map(|i| self.u16(at(i * 2)).map(|v| (v as i16).to_string())),
            ),
            4 => join((0..n.min(16)).filter_map(|i| self.u32(at(i * 4)).map(|v| v.to_string()))),
            9 => join(
                (0..n.min(16)).filter_map(|i| self.u32(at(i * 4)).map(|v| (v as i32).to_string())),
            ),
            5 | 10 => join((0..n.min(16)).filter_map(|i| {
                let (num, den) = (self.u32(at(i * 8))?, self.u32(at(i * 8 + 4))?);
                Some(if ty == 10 {
                    rational(num as i32 as f64, den as i32 as f64)
                } else {
                    rational(num as f64, den as f64)
                })
            })),
            11 => join(
                (0..n.min(16))
                    .filter_map(|i| self.u32(at(i * 4)).map(|v| f32::from_bits(v).to_string())),
            ),
            12 => join((0..n.min(16)).filter_map(|i| {
                let hi = self.u32(at(i * 8))? as u64;
                let lo = self.u32(at(i * 8 + 4))? as u64;
                let bits = if self.le {
                    (lo << 32) | hi
                } else {
                    (hi << 32) | lo
                };
                Some(f64::from_bits(bits).to_string())
            })),
            _ => format!("<{} bytes>", bytes.len()),
        };
        match describe(ifd, tag, &raw) {
            Some(d) => format!("{} ({})", raw, d),
            None => raw,
        }
    }

    fn urational(&self, at: usize) -> Option<f64> {
        let (n, d) = (self.u32(at)?, self.u32(at + 4)?);
        if d == 0 {
            None
        } else {
            Some(n as f64 / d as f64)
        }
    }
}

fn type_size(ty: u16) -> Option<usize> {
    Some(match ty {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 => 4,
        5 | 10 | 12 => 8,
        _ => return None,
    })
}

fn join(it: impl Iterator<Item = String>) -> String {
    it.collect::<Vec<_>>().join(", ")
}

fn rational(n: f64, d: f64) -> String {
    if d == 0.0 {
        return "undefined".into();
    }
    let v = n / d;
    if v.fract() == 0.0 {
        format!("{}", v)
    } else if v > 0.0 && v < 1.0 && (1.0 / v - (1.0 / v).round()).abs() < 1e-6 {
        format!("1/{}", (1.0 / v).round()) // e.g. exposure time 1/250
    } else {
        let s = format!("{:.4}", v);
        s.trim_end_matches('0').to_string()
    }
}

fn utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

fn user_comment(bytes: &[u8]) -> String {
    if bytes.len() < 8 {
        return String::new();
    }
    let (charset, text) = bytes.split_at(8);
    let s = if charset.starts_with(b"UNICODE") {
        utf16le(text)
    } else {
        String::from_utf8_lossy(text).into_owned()
    };
    s.trim_matches(|c: char| c == '\0' || c.is_whitespace())
        .to_string()
}

/// Human-readable meaning for a few enumerated tags.
fn describe(ifd: Ifd, tag: u16, raw: &str) -> Option<&'static str> {
    Some(match (ifd, tag, raw) {
        (Ifd::Main, TAG_ORIENTATION, v) => match v {
            "1" => "normal",
            "2" => "mirrored",
            "3" => "rotated 180\u{b0}",
            "4" => "mirrored vertically",
            "5" => "mirrored, rotated 270\u{b0} CW",
            "6" => "rotated 90\u{b0} CW",
            "7" => "mirrored, rotated 90\u{b0} CW",
            "8" => "rotated 270\u{b0} CW",
            _ => return None,
        },
        (Ifd::Main, TAG_RESOLUTION_UNIT, v) => match v {
            "1" => "none",
            "2" => "inches",
            "3" => "cm",
            _ => return None,
        },
        (Ifd::Exif, TAG_COLOR_SPACE, v) => match v {
            "1" => "sRGB",
            "2" => "Adobe RGB",
            "65535" => "uncalibrated",
            _ => return None,
        },
        _ => return None,
    })
}

/// Adds a combined decimal "Position" entry when latitude and longitude are present.
fn add_gps_position(entries: &mut Vec<Entry>) {
    let get = |tag: u16| {
        entries
            .iter()
            .find(|e| e.ifd == Ifd::Gps && e.tag == tag)
            .map(|e| e.value.clone())
    };
    let dms = |s: &str| -> Option<f64> {
        let nums: Vec<f64> = s
            .split(|c: char| !(c.is_ascii_digit() || c == '.'))
            .filter(|p| !p.is_empty())
            .filter_map(|p| p.parse().ok())
            .collect();
        (nums.len() == 3).then(|| nums[0] + nums[1] / 60.0 + nums[2] / 3600.0)
    };
    let (lat, lon) = match (
        get(2).as_deref().and_then(dms),
        get(4).as_deref().and_then(dms),
    ) {
        (Some(a), Some(b)) => (a, b),
        _ => return,
    };
    let lat = if get(1).as_deref() == Some("S") {
        -lat
    } else {
        lat
    };
    let lon = if get(3).as_deref() == Some("W") {
        -lon
    } else {
        lon
    };
    entries.push(Entry {
        ifd: Ifd::Gps,
        tag: 0xFFFF,
        name: "Position".into(),
        value: format!("{:.6}, {:.6}", lat, lon),
        kept_by_default: false,
    });
}

fn tag_name(ifd: Ifd, tag: u16) -> Option<&'static str> {
    Some(match ifd {
        Ifd::Main | Ifd::Thumbnail => match tag {
            0x00FE => "SubfileType",
            0x0100 => "ImageWidth",
            0x0101 => "ImageHeight",
            0x0102 => "BitsPerSample",
            0x0103 => "Compression",
            0x0106 => "PhotometricInterpretation",
            0x010E => "ImageDescription",
            0x010F => "Make",
            0x0110 => "Model",
            0x0112 => "Orientation",
            0x0115 => "SamplesPerPixel",
            0x011A => "XResolution",
            0x011B => "YResolution",
            0x0128 => "ResolutionUnit",
            0x0131 => "Software",
            0x0132 => "DateTime",
            0x013B => "Artist",
            0x013C => "HostComputer",
            0x0201 => "ThumbnailOffset",
            0x0202 => "ThumbnailLength",
            0x0142 => "TileWidth",
            0x0143 => "TileLength",
            0x0213 => "YCbCrPositioning",
            0x4746 => "Rating",
            0x8298 => "Copyright",
            0x9C9B => "XPTitle",
            0x9C9C => "XPComment",
            0x9C9D => "XPAuthor",
            0x9C9E => "XPKeywords",
            0x9C9F => "XPSubject",
            0xC4A5 => "PrintIM",
            _ => return None,
        },
        Ifd::Exif => match tag {
            0x829A => "ExposureTime",
            0x829D => "FNumber",
            0x8822 => "ExposureProgram",
            0x8827 => "ISO",
            0x8830 => "SensitivityType",
            0x9000 => "ExifVersion",
            0x9003 => "DateTimeOriginal",
            0x9004 => "DateTimeDigitized",
            0x9010 => "OffsetTime",
            0x9011 => "OffsetTimeOriginal",
            0x9012 => "OffsetTimeDigitized",
            0x9101 => "ComponentsConfiguration",
            0x9201 => "ShutterSpeedValue",
            0x9202 => "ApertureValue",
            0x9203 => "BrightnessValue",
            0x9204 => "ExposureBiasValue",
            0x9205 => "MaxApertureValue",
            0x9207 => "MeteringMode",
            0x9208 => "LightSource",
            0x9209 => "Flash",
            0x920A => "FocalLength",
            0x9214 => "SubjectArea",
            0x927C => "MakerNote",
            0x9286 => "UserComment",
            0x9290 => "SubSecTime",
            0x9291 => "SubSecTimeOriginal",
            0x9292 => "SubSecTimeDigitized",
            0xA000 => "FlashpixVersion",
            0xA001 => "ColorSpace",
            0xA002 => "PixelXDimension",
            0xA003 => "PixelYDimension",
            0xA217 => "SensingMethod",
            0xA300 => "FileSource",
            0xA301 => "SceneType",
            0xA401 => "CustomRendered",
            0xA402 => "ExposureMode",
            0xA403 => "WhiteBalance",
            0xA404 => "DigitalZoomRatio",
            0xA405 => "FocalLengthIn35mmFilm",
            0xA406 => "SceneCaptureType",
            0xA420 => "ImageUniqueID",
            0xA430 => "CameraOwnerName",
            0xA431 => "BodySerialNumber",
            0xA432 => "LensSpecification",
            0xA433 => "LensMake",
            0xA434 => "LensModel",
            0xA435 => "LensSerialNumber",
            0xA460 => "CompositeImage",
            _ => return None,
        },
        Ifd::Gps => match tag {
            0x00 => "GPSVersionID",
            0x01 => "GPSLatitudeRef",
            0x02 => "GPSLatitude",
            0x03 => "GPSLongitudeRef",
            0x04 => "GPSLongitude",
            0x05 => "GPSAltitudeRef",
            0x06 => "GPSAltitude",
            0x07 => "GPSTimeStamp",
            0x0B => "GPSDOP",
            0x0C => "GPSSpeedRef",
            0x0D => "GPSSpeed",
            0x10 => "GPSImgDirectionRef",
            0x11 => "GPSImgDirection",
            0x12 => "GPSMapDatum",
            0x17 => "GPSDestBearingRef",
            0x18 => "GPSDestBearing",
            0x1B => "GPSProcessingMethod",
            0x1D => "GPSDateStamp",
            0x1F => "GPSHPositioningError",
            _ => return None,
        },
        Ifd::Interop => match tag {
            0x0001 => "InteropIndex",
            0x0002 => "InteropVersion",
            _ => return None,
        },
    })
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
    fn lists_tags_with_names_and_kept_flags() {
        let mut t = b"II\x2A\0\x08\0\0\0".to_vec();
        t.extend_from_slice(&2u16.to_le_bytes());
        t.extend_from_slice(&[0x0F, 0x01, 2, 0, 4, 0, 0, 0, b'C', b'a', b'm', 0]);
        t.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0]);
        t.extend_from_slice(&[0, 0, 0, 0]);
        let e = list(&t);
        assert_eq!(e.len(), 2);
        assert_eq!(
            (
                e[0].name.as_str(),
                e[0].value.as_str(),
                e[0].kept_by_default
            ),
            ("Make", "Cam", false)
        );
        assert_eq!(e[1].name, "Orientation");
        assert!(e[1].value.starts_with("6 (rotated 90"));
        assert!(e[1].kept_by_default);
    }

    #[test]
    fn garbage_yields_nothing() {
        assert!(list(b"not a tiff").is_empty());
        assert!(Kept::read(b"not a tiff").is_empty());
        assert!(Kept::read(b"MM\0\x2A\xFF\xFF\xFF\xFF").is_empty());
    }
}
