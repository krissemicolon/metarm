//! `metarm -i`: list the metadata in a file and what stripping would do with it.
//!
//! Uses the same parsers and keep/drop rules as `strip`, so the listing always
//! matches what a real run removes.

use crate::exif;
use crate::strip::{self, Format, JpegFate, JpegPart, Malformed, Mode};

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub group: String,
    pub name: String,
    pub value: String,
    /// Whether stripping (with the given mode) keeps this item.
    pub kept: bool,
}

fn item(group: &str, name: impl Into<String>, value: impl Into<String>, kept: bool) -> Item {
    Item {
        group: group.into(),
        name: name.into(),
        value: value.into(),
        kept,
    }
}

pub fn inspect(format: Format, data: &[u8], mode: Mode) -> Result<Vec<Item>, Malformed> {
    match format {
        Format::Jpeg => inspect_jpeg(data, mode),
        Format::Png => inspect_png(data, mode),
        Format::Webp => inspect_webp(data, mode),
    }
}

/// EXIF entries; only the first EXIF block in a file can be (partially) kept.
fn exif_items(out: &mut Vec<Item>, tiff: &[u8], mode: Mode, first: bool) {
    let entries = exif::list(tiff);
    if entries.is_empty() {
        out.push(item(
            "EXIF",
            "EXIF block",
            format!("{} bytes (unreadable)", tiff.len()),
            false,
        ));
    }
    for e in entries {
        let group = match e.ifd {
            exif::Ifd::Gps => "GPS",
            _ => "EXIF",
        };
        let kept = first && mode == Mode::KeepDisplay && e.kept_by_default;
        out.push(item(group, e.name, e.value, kept));
    }
}

fn trailer_item(out: &mut Vec<Item>, trailer: &[u8]) {
    if !trailer.is_empty() {
        out.push(item(
            "File",
            "Trailing data",
            format!("{} after the end of the image", bytes(trailer.len())),
            false,
        ));
    }
}

fn bytes(n: usize) -> String {
    if n < 1024 {
        format!("{} bytes", n)
    } else if n < 1024 * 1024 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
    }
}

/// The printable identifier at the start of an APPn payload, e.g. "MPF" or "Ducky".
fn app_identifier(payload: &[u8]) -> String {
    let id: String = payload
        .iter()
        .take_while(|&&b| b != 0)
        .take(32)
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '?'
            }
        })
        .collect();
    if id.is_empty() {
        "(unnamed)".into()
    } else {
        id
    }
}

// ---------------------------------------------------------------- JPEG ----

const XMP_ID: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
const XMP_EXT_ID: &[u8] = b"http://ns.adobe.com/xmp/extension/\0";

fn inspect_jpeg(data: &[u8], mode: Mode) -> Result<Vec<Item>, Malformed> {
    let mut out = Vec::new();
    let mut exif_seen = false;
    let mut icc = Vec::new();

    for part in strip::jpeg_parts(data)? {
        let (marker, payload) = match part {
            JpegPart::Segment { marker, payload } => (marker, payload),
            JpegPart::Trailer(t) => {
                trailer_item(&mut out, t);
                continue;
            }
            _ => continue,
        };
        match strip::jpeg_fate(marker, payload) {
            JpegFate::JfifWithoutThumbnail => {
                let unit = match payload[7] {
                    1 => " dpi",
                    2 => " dots/cm",
                    _ => " (aspect ratio)",
                };
                let xd = u16::from_be_bytes([payload[8], payload[9]]);
                let yd = u16::from_be_bytes([payload[10], payload[11]]);
                out.push(item(
                    "JFIF",
                    "Density",
                    format!("{}x{}{}", xd, yd, unit),
                    true,
                ));
                if payload[12] > 0 && payload[13] > 0 {
                    let size = format!("{}x{} preview image", payload[12], payload[13]);
                    out.push(item("JFIF", "Thumbnail", size, false));
                }
            }
            JpegFate::RebuildExif => {
                exif_items(&mut out, &payload[6..], mode, !exif_seen);
                exif_seen = true;
            }
            JpegFate::Keep => {
                if marker == 0xE2 && payload.len() > 14 {
                    icc.extend_from_slice(&payload[14..]);
                }
            }
            JpegFate::Drop => match marker {
                0xE1 if payload.starts_with(XMP_ID) => {
                    xmp_items(&mut out, &payload[XMP_ID.len()..])
                }
                0xE1 if payload.starts_with(XMP_EXT_ID) => {
                    out.push(item("XMP", "Extended XMP", bytes(payload.len()), false))
                }
                0xED if payload.starts_with(b"Photoshop 3.0\0") => {
                    photoshop_items(&mut out, &payload[14..])
                }
                0xE0 if payload.starts_with(b"JFXX\0") => out.push(item(
                    "JFIF",
                    "Thumbnail (JFXX)",
                    bytes(payload.len()),
                    false,
                )),
                0xE2 if payload.starts_with(b"MPF\0") => out.push(item(
                    "APP2",
                    "MPF",
                    format!("index of extra images, {}", bytes(payload.len())),
                    false,
                )),
                0xFE => out.push(item(
                    "Comment",
                    "Comment",
                    String::from_utf8_lossy(payload).into_owned(),
                    false,
                )),
                _ => out.push(item(
                    &format!("APP{}", marker - 0xE0),
                    app_identifier(payload),
                    bytes(payload.len()),
                    false,
                )),
            },
        }
    }
    if !icc.is_empty() {
        out.push(icc_item(&icc));
    }
    Ok(out)
}

// ----------------------------------------------------------------- PNG ----

fn inspect_png(data: &[u8], mode: Mode) -> Result<Vec<Item>, Malformed> {
    let (chunks, trailer) = strip::png_chunks(data)?;
    let mut out = Vec::new();
    let mut exif_seen = false;
    for c in chunks {
        let d = c.data;
        match &c.ty {
            b"eXIf" => {
                exif_items(&mut out, d, mode, !exif_seen);
                exif_seen = true;
            }
            b"tEXt" => {
                let (key, text) = split_nul(d);
                out.push(item("PNG text", latin1(key), latin1(text), false));
            }
            b"zTXt" => {
                let (key, rest) = split_nul(d);
                let v = format!("<compressed, {}>", bytes(rest.len().saturating_sub(1)));
                out.push(item("PNG text", latin1(key), v, false));
            }
            b"iTXt" => {
                let (key, rest) = split_nul(d);
                let compressed = rest.first() == Some(&1);
                let rest = rest.get(2..).unwrap_or(&[]);
                let (_lang, rest) = split_nul(rest);
                let (_translated, text) = split_nul(rest);
                let v = if compressed {
                    format!("<compressed, {}>", bytes(text.len()))
                } else {
                    String::from_utf8_lossy(text).into_owned()
                };
                out.push(item("PNG text", String::from_utf8_lossy(key), v, false));
            }
            b"tIME" if d.len() == 7 => {
                let v = format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
                    u16::from_be_bytes([d[0], d[1]]),
                    d[2],
                    d[3],
                    d[4],
                    d[5],
                    d[6]
                );
                out.push(item("PNG", "Modified", v, false));
            }
            b"iCCP" => {
                let (name, _) = split_nul(d);
                out.push(item("ICC", "Profile", latin1(name), true));
            }
            b"pHYs" if d.len() == 9 => {
                let x = u32::from_be_bytes([d[0], d[1], d[2], d[3]]);
                let y = u32::from_be_bytes([d[4], d[5], d[6], d[7]]);
                let v = if d[8] == 1 {
                    format!("{:.0}x{:.0} dpi", x as f64 * 0.0254, y as f64 * 0.0254)
                } else {
                    format!("{}:{} (aspect ratio)", x, y)
                };
                out.push(item("PNG", "Density", v, true));
            }
            ty if !strip::png_keeps(ty) => out.push(item(
                "PNG",
                format!("{} chunk", String::from_utf8_lossy(ty)),
                bytes(d.len()),
                false,
            )),
            _ => {}
        }
    }
    trailer_item(&mut out, trailer);
    Ok(out)
}

fn split_nul(d: &[u8]) -> (&[u8], &[u8]) {
    match d.iter().position(|&b| b == 0) {
        Some(i) => (&d[..i], &d[i + 1..]),
        None => (d, &[]),
    }
}

fn latin1(d: &[u8]) -> String {
    d.iter().map(|&b| b as char).collect()
}

// ---------------------------------------------------------------- WebP ----

fn inspect_webp(data: &[u8], mode: Mode) -> Result<Vec<Item>, Malformed> {
    let (chunks, trailer) = strip::webp_chunks(data)?;
    let mut out = Vec::new();
    let mut exif_seen = false;
    for c in chunks {
        match &c.ty {
            b"EXIF" => {
                exif_items(&mut out, c.data, mode, !exif_seen);
                exif_seen = true;
            }
            b"XMP " => xmp_items(&mut out, c.data),
            b"ICCP" => out.push(icc_item(c.data)),
            ty if !strip::webp_keeps(ty) => out.push(item(
                "WebP",
                format!("{} chunk", String::from_utf8_lossy(ty).trim_end()),
                bytes(c.data.len()),
                false,
            )),
            _ => {}
        }
    }
    trailer_item(&mut out, trailer);
    Ok(out)
}

// ----------------------------------------------------------------- ICC ----

fn icc_item(profile: &[u8]) -> Item {
    let desc = icc_description(profile).unwrap_or_else(|| bytes(profile.len()));
    item("ICC", "Profile", desc, true)
}

/// Reads the profile description ('desc' tag) of an ICC profile.
fn icc_description(p: &[u8]) -> Option<String> {
    let u32_at = |at: usize| -> Option<u32> {
        Some(u32::from_be_bytes(p.get(at..at + 4)?.try_into().ok()?))
    };
    let count = u32_at(128)? as usize;
    for i in 0..count.min(256) {
        let e = 132 + i * 12;
        if p.get(e..e + 4)? != b"desc" {
            continue;
        }
        let off = u32_at(e + 4)? as usize;
        let tag = p.get(off..)?;
        return match tag.get(0..4)? {
            b"desc" => {
                let n = u32_at(off + 8)? as usize;
                let s = tag.get(12..12 + n)?;
                Some(latin1(split_nul(s).0))
            }
            b"mluc" => {
                let len = u32_at(off + 20)? as usize;
                let start = u32_at(off + 24)? as usize;
                let s = tag.get(start..start + len)?;
                let units: Vec<u16> = s
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_be_bytes(*c))
                    .collect();
                Some(String::from_utf16_lossy(&units))
            }
            _ => None,
        };
    }
    None
}

// ----------------------------------------------------------------- XMP ----

/// Pulls simple properties out of an XMP packet: `prefix:Name="value"`
/// attributes and `<prefix:Name>value</prefix:Name>` elements, with values of
/// `rdf:li` list items attributed to their enclosing property.
fn xmp_items(out: &mut Vec<Item>, packet: &[u8]) {
    let xml = String::from_utf8_lossy(packet);
    let mut props: Vec<(String, Vec<String>)> = Vec::new();
    let mut add = |name: &str, value: &str| {
        let value = xml_unescape(value.trim());
        if value.is_empty() {
            return;
        }
        match props.iter_mut().find(|(n, _)| n == name) {
            Some((_, vs)) => vs.push(value),
            None => props.push((name.to_string(), vec![value])),
        }
    };
    let skip = |name: &str| {
        name.starts_with("rdf:")
            || name.starts_with("x:")
            || name.starts_with("xmlns")
            || name.starts_with("xml:")
            || !name.contains(':')
    };

    let mut current_prop: Option<String> = None;
    let mut rest: &str = &xml;
    while let Some(lt) = rest.find('<') {
        rest = &rest[lt + 1..];
        let gt = match rest.find('>') {
            Some(i) => i,
            None => break,
        };
        let tag = &rest[..gt];
        let after = &rest[gt + 1..];
        if tag.starts_with('/') || tag.starts_with('?') || tag.starts_with('!') {
            rest = after;
            continue;
        }
        let name_end = tag
            .find(|c: char| c.is_whitespace() || c == '/')
            .unwrap_or(tag.len());
        let name = &tag[..name_end];

        // Attributes: name="value"
        let mut attrs = &tag[name_end..];
        while let Some(eq) = attrs.find("=\"") {
            let key = attrs[..eq]
                .trim()
                .rsplit(char::is_whitespace)
                .next()
                .unwrap_or("");
            let val_rest = &attrs[eq + 2..];
            let close = match val_rest.find('"') {
                Some(i) => i,
                None => break,
            };
            if !skip(key) {
                add(key, &val_rest[..close]);
            }
            attrs = &val_rest[close + 1..];
        }

        // Text content: <name>text<
        let text = after.split('<').next().unwrap_or("");
        let self_closing = tag.ends_with('/');
        if name == "rdf:li" {
            if let Some(p) = &current_prop {
                if !self_closing {
                    add(p, text);
                }
            }
        } else if !skip(name) {
            current_prop = Some(name.to_string());
            if !self_closing {
                add(name, text);
            }
        }
        rest = after;
    }

    if props.is_empty() {
        out.push(item("XMP", "Packet", bytes(packet.len()), false));
    }
    for (name, values) in props {
        out.push(item("XMP", name, values.join(", "), false));
    }
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#xA;", " ")
        .replace("&amp;", "&")
}

// ---------------------------------------------------------- Photoshop ----

/// Lists IPTC datasets from Photoshop image resources (JPEG APP13).
fn photoshop_items(out: &mut Vec<Item>, mut d: &[u8]) {
    let mut other = 0;
    while d.len() >= 12 && d.starts_with(b"8BIM") {
        let id = u16::from_be_bytes([d[4], d[5]]);
        let name_len = d[6] as usize;
        let name_total = (1 + name_len + 1) & !1; // padded to even
        let size_at = 6 + name_total;
        let size = match d.get(size_at..size_at + 4) {
            Some(b) => u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize,
            None => break,
        };
        let data = match d.get(size_at + 4..size_at + 4 + size) {
            Some(x) => x,
            None => break,
        };
        if id == 0x0404 {
            iptc_items(out, data);
        } else {
            other += 1;
        }
        d = d.get(size_at + 4 + ((size + 1) & !1)..).unwrap_or(&[]);
    }
    if other > 0 {
        out.push(item(
            "Photoshop",
            "Image resources",
            format!("{} other resource block(s)", other),
            false,
        ));
    }
}

fn iptc_items(out: &mut Vec<Item>, mut d: &[u8]) {
    let mut fields: Vec<(String, Vec<String>)> = Vec::new();
    while d.len() >= 5 && d[0] == 0x1C {
        let (record, dataset) = (d[1], d[2]);
        let size = u16::from_be_bytes([d[3], d[4]]) as usize;
        if size & 0x8000 != 0 {
            break; // extended dataset length; rare, stop here
        }
        let value = match d.get(5..5 + size) {
            Some(v) => String::from_utf8_lossy(v).into_owned(),
            None => break,
        };
        d = &d[5 + size..];
        if record == 2 && dataset == 0 {
            continue; // record version
        }
        let name = iptc_name(record, dataset)
            .map(String::from)
            .unwrap_or_else(|| format!("{}:{}", record, dataset));
        match fields.iter_mut().find(|(n, _)| *n == name) {
            Some((_, vs)) => vs.push(value),
            None => fields.push((name, vec![value])),
        }
    }
    for (name, values) in fields {
        out.push(item("IPTC", name, values.join(", "), false));
    }
}

fn iptc_name(record: u8, dataset: u8) -> Option<&'static str> {
    if record != 2 {
        return None;
    }
    Some(match dataset {
        5 => "ObjectName",
        25 => "Keywords",
        40 => "SpecialInstructions",
        55 => "DateCreated",
        60 => "TimeCreated",
        65 => "OriginatingProgram",
        70 => "ProgramVersion",
        80 => "By-line",
        85 => "By-lineTitle",
        90 => "City",
        92 => "Sub-location",
        95 => "Province-State",
        100 => "CountryCode",
        101 => "Country",
        105 => "Headline",
        110 => "Credit",
        115 => "Source",
        116 => "CopyrightNotice",
        120 => "Caption-Abstract",
        122 => "Writer-Editor",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xmp_extracts_attributes_elements_and_lists() {
        let xmp = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF>
            <rdf:Description xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:CreatorTool="Lightroom &amp; Co">
              <dc:creator><rdf:Seq><rdf:li>Jane Doe</rdf:li></rdf:Seq></dc:creator>
              <dc:subject><rdf:Bag><rdf:li>beach</rdf:li><rdf:li>sunset</rdf:li></rdf:Bag></dc:subject>
              <photoshop:City>Zurich</photoshop:City>
            </rdf:Description></rdf:RDF></x:xmpmeta>"#;
        let mut out = Vec::new();
        xmp_items(&mut out, xmp);
        let got: Vec<(&str, &str)> = out
            .iter()
            .map(|i| (i.name.as_str(), i.value.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("xmp:CreatorTool", "Lightroom & Co"),
                ("dc:creator", "Jane Doe"),
                ("dc:subject", "beach, sunset"),
                ("photoshop:City", "Zurich"),
            ]
        );
    }

    #[test]
    fn iptc_from_photoshop_resources() {
        let mut iptc = vec![0x1C, 2, 80, 0, 4];
        iptc.extend_from_slice(b"Jane");
        iptc.extend_from_slice(&[0x1C, 2, 25, 0, 1, b'a', 0x1C, 2, 25, 0, 1, b'b']);
        let mut res = b"8BIM\x04\x04\0\0".to_vec();
        res.extend_from_slice(&(iptc.len() as u32).to_be_bytes());
        res.extend_from_slice(&iptc);
        let mut out = Vec::new();
        photoshop_items(&mut out, &res);
        assert_eq!(out.len(), 2);
        assert_eq!(
            (out[0].name.as_str(), out[0].value.as_str()),
            ("By-line", "Jane")
        );
        assert_eq!(
            (out[1].name.as_str(), out[1].value.as_str()),
            ("Keywords", "a, b")
        );
    }
}
