//! Format-specific metadata stripping.
//!
//! Each format is first split into its structural parts (JPEG segments,
//! PNG chunks, RIFF chunks). The keep/drop decision for each part lives in
//! one function per format, shared by the stripper and by `info`, so what
//! `metarm -i` reports is exactly what stripping does.
//!
//! Format detection is done by magic bytes, never by file extension.

use crate::exif::Kept;
use std::fmt;

/// What to do with the small set of display-related EXIF tags
/// (orientation, resolution, colour space).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Keep the display-related tags in a freshly rebuilt EXIF block.
    KeepDisplay,
    /// Drop all EXIF.
    All,
}

/// Reads the whitelisted tags from an EXIF block and rebuilds a minimal one.
/// Returns `None` when there is nothing worth keeping (or in `Mode::All`).
fn rebuilt_exif(mode: Mode, tiff: &[u8]) -> Option<Vec<u8>> {
    if mode == Mode::All {
        return None;
    }
    let kept = Kept::read(tiff);
    if kept.is_empty() {
        None
    } else {
        Some(kept.to_tiff())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Jpeg,
    Png,
    Webp,
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Format::Jpeg => "JPEG",
            Format::Png => "PNG",
            Format::Webp => "WebP",
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Malformed(pub &'static str);

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "malformed file: {}", self.0)
    }
}

pub fn detect(data: &[u8]) -> Option<Format> {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(Format::Jpeg)
    } else if data.starts_with(PNG_SIG) {
        Some(Format::Png)
    } else if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some(Format::Webp)
    } else {
        None
    }
}

pub fn strip(format: Format, data: &[u8], mode: Mode) -> Result<Vec<u8>, Malformed> {
    match format {
        Format::Jpeg => strip_jpeg(data, mode),
        Format::Png => strip_png(data, mode),
        Format::Webp => strip_webp(data, mode),
    }
}

// ---------------------------------------------------------------- JPEG ----

pub enum JpegPart<'a> {
    /// A marker without a length field (RSTn, TEM).
    Standalone(u8),
    /// A marker segment; `payload` excludes the 2-byte length field.
    Segment {
        marker: u8,
        payload: &'a [u8],
    },
    /// Entropy-coded image data following an SOS segment.
    Scan(&'a [u8]),
    Eoi,
    /// Bytes after EOI (vendor trailers, appended thumbnails/videos, ...).
    Trailer(&'a [u8]),
}

pub fn jpeg_parts(data: &[u8]) -> Result<Vec<JpegPart<'_>>, Malformed> {
    if !data.starts_with(&[0xFF, 0xD8]) {
        return Err(Malformed("missing JPEG SOI marker"));
    }
    let mut parts = Vec::new();
    let mut pos = 2;

    loop {
        // Find next marker: 0xFF followed by a non-0xFF byte (0xFF fill bytes are allowed).
        if pos >= data.len() || data[pos] != 0xFF {
            return Err(Malformed("expected JPEG marker"));
        }
        while pos < data.len() && data[pos] == 0xFF {
            pos += 1;
        }
        if pos >= data.len() {
            return Err(Malformed("truncated JPEG marker"));
        }
        let marker = data[pos];
        pos += 1;

        match marker {
            0xD9 => {
                parts.push(JpegPart::Eoi);
                if pos < data.len() {
                    parts.push(JpegPart::Trailer(&data[pos..]));
                }
                return Ok(parts);
            }
            0x01 | 0xD0..=0xD7 => {
                parts.push(JpegPart::Standalone(marker));
                continue;
            }
            _ => {}
        }

        if pos + 2 > data.len() {
            return Err(Malformed("truncated JPEG segment length"));
        }
        let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        if len < 2 || pos + len > data.len() {
            return Err(Malformed("invalid JPEG segment length"));
        }
        parts.push(JpegPart::Segment {
            marker,
            payload: &data[pos + 2..pos + len],
        });
        pos += len;

        if marker == 0xDA {
            // SOS: entropy-coded data runs until the next real marker.
            let start = pos;
            loop {
                if pos + 1 >= data.len() {
                    // No EOI; keep what's there (truncated but still decodable by most readers).
                    parts.push(JpegPart::Scan(&data[start..]));
                    return Ok(parts);
                }
                if data[pos] == 0xFF {
                    let next = data[pos + 1];
                    // 0x00 = stuffed byte, D0-D7 = restart markers, FF = fill.
                    if next != 0x00 && !(0xD0..=0xD7).contains(&next) && next != 0xFF {
                        break;
                    }
                }
                pos += 1;
            }
            parts.push(JpegPart::Scan(&data[start..pos]));
        }
    }
}

/// What stripping does with a JPEG segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JpegFate {
    Keep,
    Drop,
    /// JFIF header: kept, but any embedded thumbnail is removed.
    JfifWithoutThumbnail,
    /// EXIF: the first block is rebuilt with display tags only; later ones are dropped.
    RebuildExif,
}

/// Keeps only what is needed to decode the image correctly:
/// - APP0 JFIF header (version + pixel density), with any embedded thumbnail removed
/// - APP1 EXIF, rebuilt from scratch with only display tags (unless `Mode::All`)
/// - APP2 only if it is an ICC profile (other APP2 users, e.g. FPXR/MPF, are dropped)
/// - APP14 "Adobe" (needed to decode CMYK/YCCK colour correctly)
///
/// Dropped: XMP, JFXX thumbnails, the remaining APPn segments (IPTC/Photoshop,
/// maker data, ...), COM comments, and anything trailing after EOI.
pub fn jpeg_fate(marker: u8, payload: &[u8]) -> JpegFate {
    match marker {
        0xE0 if payload.len() >= 14 && payload.starts_with(b"JFIF\0") => {
            JpegFate::JfifWithoutThumbnail
        }
        0xE1 if payload.starts_with(b"Exif\0\0") => JpegFate::RebuildExif,
        0xE2 if payload.starts_with(b"ICC_PROFILE\0") => JpegFate::Keep,
        0xEE if payload.starts_with(b"Adobe") => JpegFate::Keep,
        0xE0..=0xEF | 0xFE => JpegFate::Drop,
        _ => JpegFate::Keep,
    }
}

fn push_jpeg_segment(out: &mut Vec<u8>, marker: u8, payload: &[u8]) {
    out.extend_from_slice(&[0xFF, marker]);
    out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(payload);
}

fn strip_jpeg(data: &[u8], mode: Mode) -> Result<Vec<u8>, Malformed> {
    let mut out = Vec::with_capacity(data.len());
    out.extend_from_slice(&[0xFF, 0xD8]);
    let mut exif_done = false;

    for part in jpeg_parts(data)? {
        match part {
            JpegPart::Standalone(m) => out.extend_from_slice(&[0xFF, m]),
            JpegPart::Scan(bytes) => out.extend_from_slice(bytes),
            JpegPart::Eoi => out.extend_from_slice(&[0xFF, 0xD9]),
            JpegPart::Trailer(_) => {}
            JpegPart::Segment { marker, payload } => match jpeg_fate(marker, payload) {
                JpegFate::Keep => push_jpeg_segment(&mut out, marker, payload),
                JpegFate::Drop => {}
                JpegFate::JfifWithoutThumbnail => {
                    // JFIF: version(2) units(1) density(4) thumb_w(1) thumb_h(1) thumb...
                    let mut jfif = payload[..12].to_vec();
                    jfif.extend_from_slice(&[0, 0]);
                    push_jpeg_segment(&mut out, marker, &jfif);
                }
                JpegFate::RebuildExif => {
                    if !exif_done {
                        exif_done = true;
                        if let Some(tiff) = rebuilt_exif(mode, &payload[6..]) {
                            push_jpeg_segment(
                                &mut out,
                                marker,
                                &[b"Exif\0\0".as_ref(), &tiff].concat(),
                            );
                        }
                    }
                }
            },
        }
    }
    Ok(out)
}

// ----------------------------------------------------------------- PNG ----

const PNG_SIG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Ancillary chunks that affect how the image is rendered. Every other
/// ancillary chunk (tEXt, zTXt, iTXt, tIME, private chunks, ...) is dropped.
/// eXIf is rebuilt with display tags only (or dropped in `Mode::All`).
/// Critical chunks (uppercase first letter) are always kept.
const PNG_KEEP_ANCILLARY: &[&[u8; 4]] = &[
    b"tRNS", b"gAMA", b"cHRM", b"sRGB", b"iCCP", b"sBIT", b"bKGD", b"pHYs", b"hIST", b"sPLT",
    b"cICP", b"mDCV", b"cLLI", // colour / display
    b"acTL", b"fcTL", b"fdAT", // APNG animation
];

pub struct PngChunk<'a> {
    pub ty: [u8; 4],
    pub data: &'a [u8],
    /// The whole chunk: length, type, data and CRC.
    pub raw: &'a [u8],
}

/// Returns the chunks up to and including IEND, plus any bytes after IEND.
pub fn png_chunks(data: &[u8]) -> Result<(Vec<PngChunk<'_>>, &[u8]), Malformed> {
    if !data.starts_with(PNG_SIG) {
        return Err(Malformed("missing PNG signature"));
    }
    let mut chunks = Vec::new();
    let mut pos = PNG_SIG.len();
    while pos < data.len() {
        if pos + 8 > data.len() {
            return Err(Malformed("truncated PNG chunk header"));
        }
        let len =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        let ty: [u8; 4] = [data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]];
        let end = pos
            .checked_add(12)
            .and_then(|v| v.checked_add(len))
            .filter(|&e| e <= data.len())
            .ok_or(Malformed("truncated PNG chunk"))?;
        chunks.push(PngChunk {
            ty,
            data: &data[pos + 8..end - 4],
            raw: &data[pos..end],
        });
        pos = end;
        if &ty == b"IEND" {
            return Ok((chunks, &data[pos..]));
        }
    }
    Err(Malformed("missing PNG IEND chunk"))
}

/// Whether stripping keeps a PNG chunk as-is (eXIf is handled separately).
pub fn png_keeps(ty: &[u8; 4]) -> bool {
    ty[0].is_ascii_uppercase() || PNG_KEEP_ANCILLARY.contains(&ty)
}

fn png_crc(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn strip_png(data: &[u8], mode: Mode) -> Result<Vec<u8>, Malformed> {
    let (chunks, _trailer) = png_chunks(data)?;
    let mut out = Vec::with_capacity(data.len());
    out.extend_from_slice(PNG_SIG);
    for c in chunks {
        if &c.ty == b"eXIf" {
            if let Some(tiff) = rebuilt_exif(mode, c.data) {
                let start = out.len();
                out.extend_from_slice(&(tiff.len() as u32).to_be_bytes());
                out.extend_from_slice(b"eXIf");
                out.extend_from_slice(&tiff);
                let crc = png_crc(&out[start + 4..]);
                out.extend_from_slice(&crc.to_be_bytes());
            }
        } else if png_keeps(&c.ty) {
            out.extend_from_slice(c.raw);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- WebP ----

const WEBP_KEEP: &[&[u8; 4]] = &[
    b"VP8 ", b"VP8L", b"VP8X", b"ALPH", b"ANIM", b"ANMF", b"ICCP",
];
const VP8X_FLAG_EXIF: u8 = 0x08;
const VP8X_FLAG_XMP: u8 = 0x04;

pub struct RiffChunk<'a> {
    pub ty: [u8; 4],
    pub data: &'a [u8],
    /// The whole chunk: type, size, data and padding byte.
    pub raw: &'a [u8],
}

/// Returns the chunks inside the RIFF container, plus any bytes after it.
pub fn webp_chunks(data: &[u8]) -> Result<(Vec<RiffChunk<'_>>, &[u8]), Malformed> {
    if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WEBP" {
        return Err(Malformed("missing RIFF/WEBP header"));
    }
    let riff_len = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
    let riff_end = 8usize
        .checked_add(riff_len)
        .filter(|&e| e <= data.len())
        .ok_or(Malformed("RIFF size exceeds file size"))?;

    let mut chunks = Vec::new();
    let mut pos = 12;
    while pos < riff_end {
        if pos + 8 > riff_end {
            return Err(Malformed("truncated WebP chunk header"));
        }
        let ty: [u8; 4] = [data[pos], data[pos + 1], data[pos + 2], data[pos + 3]];
        let len = u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
            as usize;
        let end = pos
            .checked_add(8 + len + (len & 1))
            .filter(|&e| e <= riff_end)
            .ok_or(Malformed("truncated WebP chunk"))?;
        chunks.push(RiffChunk {
            ty,
            data: &data[pos + 8..pos + 8 + len],
            raw: &data[pos..end],
        });
        pos = end;
    }
    Ok((chunks, &data[riff_end..]))
}

/// Whether stripping keeps a WebP chunk as-is (EXIF is handled separately).
pub fn webp_keeps(ty: &[u8; 4]) -> bool {
    WEBP_KEEP.contains(&ty)
}

fn strip_webp(data: &[u8], mode: Mode) -> Result<Vec<u8>, Malformed> {
    let (chunks, _trailer) = webp_chunks(data)?;
    let mut body = Vec::with_capacity(data.len());
    body.extend_from_slice(b"WEBP");
    let mut vp8x_flags_at = None;
    let mut exif_done = false;
    let mut wrote_exif = false;

    for c in chunks {
        if &c.ty == b"EXIF" {
            if !exif_done {
                exif_done = true;
                if let Some(tiff) = rebuilt_exif(mode, c.data) {
                    wrote_exif = true;
                    body.extend_from_slice(b"EXIF");
                    body.extend_from_slice(&(tiff.len() as u32).to_le_bytes());
                    body.extend_from_slice(&tiff);
                    if tiff.len() % 2 == 1 {
                        body.push(0);
                    }
                }
            }
        } else if webp_keeps(&c.ty) {
            if &c.ty == b"VP8X" && !c.data.is_empty() {
                vp8x_flags_at = Some(body.len() + 8);
            }
            body.extend_from_slice(c.raw);
        }
    }
    match vp8x_flags_at {
        Some(at) => {
            body[at] &= !(VP8X_FLAG_EXIF | VP8X_FLAG_XMP);
            if wrote_exif {
                body[at] |= VP8X_FLAG_EXIF;
            }
        }
        // EXIF is only valid in the extended (VP8X) format.
        None if wrote_exif => return Err(Malformed("EXIF chunk without VP8X header")),
        None => {}
    }

    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_chunk(ty: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut c = (data.len() as u32).to_be_bytes().to_vec();
        c.extend_from_slice(ty);
        c.extend_from_slice(data);
        c.extend_from_slice(&[0, 0, 0, 0]); // CRC not validated by stripper
        c
    }

    #[test]
    fn png_drops_text_and_exif() {
        let mut f = PNG_SIG.to_vec();
        f.extend(png_chunk(b"IHDR", &[0; 13]));
        f.extend(png_chunk(b"tEXt", b"Author\0me"));
        f.extend(png_chunk(b"eXIf", b"MM\0*"));
        f.extend(png_chunk(b"gAMA", &[0, 0, 0, 1]));
        f.extend(png_chunk(b"IDAT", &[1, 2, 3]));
        f.extend(png_chunk(b"IEND", &[]));
        f.extend_from_slice(b"trailing junk");

        let out = strip_png(&f, Mode::All).unwrap();
        let mut want = PNG_SIG.to_vec();
        want.extend(png_chunk(b"IHDR", &[0; 13]));
        want.extend(png_chunk(b"gAMA", &[0, 0, 0, 1]));
        want.extend(png_chunk(b"IDAT", &[1, 2, 3]));
        want.extend(png_chunk(b"IEND", &[]));
        assert_eq!(out, want);
    }

    fn jpeg_seg(marker: u8, payload: &[u8]) -> Vec<u8> {
        let mut s = vec![0xFF, marker];
        s.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        s.extend_from_slice(payload);
        s
    }

    #[test]
    fn jpeg_drops_exif_and_comments() {
        let mut f = vec![0xFF, 0xD8];
        f.extend(jpeg_seg(0xE0, b"JFIF\0\x01\x01\x01\0\x48\0\x48\x01\x01RGB"));
        f.extend(jpeg_seg(0xE1, b"Exif\0\0secret gps"));
        f.extend(jpeg_seg(0xE2, b"ICC_PROFILE\0\x01\x01data"));
        f.extend(jpeg_seg(0xE2, b"MPF\0junk"));
        f.extend(jpeg_seg(0xFE, b"comment"));
        f.extend(jpeg_seg(0xDB, &[0; 5]));
        f.extend(jpeg_seg(0xDA, &[1, 2, 3]));
        f.extend_from_slice(&[0x11, 0xFF, 0x00, 0x22, 0xFF, 0xD3, 0x33]); // scan data
        f.extend_from_slice(&[0xFF, 0xD9]);
        f.extend_from_slice(b"trailer");

        let out = strip_jpeg(&f, Mode::All).unwrap();
        let mut want = vec![0xFF, 0xD8];
        want.extend(jpeg_seg(0xE0, b"JFIF\0\x01\x01\x01\0\x48\0\x48\0\0"));
        want.extend(jpeg_seg(0xE2, b"ICC_PROFILE\0\x01\x01data"));
        want.extend(jpeg_seg(0xDB, &[0; 5]));
        want.extend(jpeg_seg(0xDA, &[1, 2, 3]));
        want.extend_from_slice(&[0x11, 0xFF, 0x00, 0x22, 0xFF, 0xD3, 0x33]);
        want.extend_from_slice(&[0xFF, 0xD9]);
        assert_eq!(out, want);
    }

    fn webp_chunk(ty: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut c = ty.to_vec();
        c.extend_from_slice(&(data.len() as u32).to_le_bytes());
        c.extend_from_slice(data);
        if data.len() % 2 == 1 {
            c.push(0);
        }
        c
    }

    fn riff(body: Vec<u8>) -> Vec<u8> {
        let mut f = b"RIFF".to_vec();
        f.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
        f.extend_from_slice(b"WEBP");
        f.extend(body);
        f
    }

    #[test]
    fn webp_drops_exif_xmp_and_clears_flags() {
        let mut body = webp_chunk(b"VP8X", &[0x0C, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        body.extend(webp_chunk(b"VP8 ", &[1, 2, 3]));
        body.extend(webp_chunk(b"EXIF", b"secret"));
        body.extend(webp_chunk(b"XMP ", b"<x/>"));
        let out = strip_webp(&riff(body), Mode::All).unwrap();

        let mut want = webp_chunk(b"VP8X", &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        want.extend(webp_chunk(b"VP8 ", &[1, 2, 3]));
        assert_eq!(out, riff(want));
    }

    fn exif_with_orientation(o: u16) -> Vec<u8> {
        // Little-endian TIFF with Make="Cam" and Orientation=o.
        let mut t = b"II\x2A\0\x08\0\0\0".to_vec();
        t.extend_from_slice(&2u16.to_le_bytes());
        t.extend_from_slice(&[0x0F, 0x01, 2, 0, 4, 0, 0, 0, b'C', b'a', b'm', 0]);
        t.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0]);
        t.extend_from_slice(&o.to_le_bytes());
        t.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        t
    }

    #[test]
    fn jpeg_keeps_orientation_by_default() {
        let mut f = vec![0xFF, 0xD8];
        f.extend(jpeg_seg(
            0xE1,
            &[b"Exif\0\0".as_ref(), &exif_with_orientation(6)].concat(),
        ));
        f.extend(jpeg_seg(0xDA, &[1]));
        f.extend_from_slice(&[0xFF, 0xD9]);

        let out = strip_jpeg(&f, Mode::KeepDisplay).unwrap();
        let kept = Kept {
            orientation: Some(6),
            ..Kept::default()
        };
        let mut want = vec![0xFF, 0xD8];
        want.extend(jpeg_seg(
            0xE1,
            &[b"Exif\0\0".as_ref(), &kept.to_tiff()].concat(),
        ));
        want.extend(jpeg_seg(0xDA, &[1]));
        want.extend_from_slice(&[0xFF, 0xD9]);
        assert_eq!(out, want);
        // Idempotent.
        assert_eq!(strip_jpeg(&out, Mode::KeepDisplay).unwrap(), out);
    }

    #[test]
    fn png_rebuilds_exif_with_valid_crc() {
        let mut f = PNG_SIG.to_vec();
        f.extend(png_chunk(b"IHDR", &[0; 13]));
        f.extend(png_chunk(b"eXIf", &exif_with_orientation(8)));
        f.extend(png_chunk(b"IEND", &[]));
        let out = strip_png(&f, Mode::KeepDisplay).unwrap();
        let tiff = Kept {
            orientation: Some(8),
            ..Kept::default()
        }
        .to_tiff();
        let at = PNG_SIG.len() + 25;
        assert_eq!(&out[at + 4..at + 8], b"eXIf");
        assert_eq!(&out[at + 8..at + 8 + tiff.len()], &tiff[..]);
        let crc = png_crc(&out[at + 4..at + 8 + tiff.len()]);
        assert_eq!(
            &out[at + 8 + tiff.len()..at + 12 + tiff.len()],
            &crc.to_be_bytes()
        );
        assert_eq!(png_crc(b"IEND"), 0xAE42_6082);
    }

    #[test]
    fn webp_keeps_exif_flag_when_orientation_kept() {
        let mut body = webp_chunk(b"VP8X", &[0x0C, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        body.extend(webp_chunk(b"VP8 ", &[1, 2, 3]));
        body.extend(webp_chunk(b"EXIF", &exif_with_orientation(3)));
        body.extend(webp_chunk(b"XMP ", b"<x/>"));
        let out = strip_webp(&riff(body), Mode::KeepDisplay).unwrap();

        let tiff = Kept {
            orientation: Some(3),
            ..Kept::default()
        }
        .to_tiff();
        let mut want = webp_chunk(b"VP8X", &[0x08, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        want.extend(webp_chunk(b"VP8 ", &[1, 2, 3]));
        want.extend(webp_chunk(b"EXIF", &tiff));
        assert_eq!(out, riff(want));
    }

    #[test]
    fn detect_formats() {
        assert_eq!(detect(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(Format::Jpeg));
        assert_eq!(detect(PNG_SIG), Some(Format::Png));
        assert_eq!(detect(b"RIFF\0\0\0\0WEBP"), Some(Format::Webp));
        assert_eq!(detect(b"hello"), None);
    }
}
