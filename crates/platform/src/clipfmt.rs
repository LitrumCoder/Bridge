//! Conversions between wire clipboard formats and what OSes store: line ends, the
//! Windows `HTML Format` envelope, PNG ↔ DIB. Pure functions, tested on every OS.

/// Images larger than this are refused before decoding (a PNG bomb is tiny on the wire).
pub const MAX_IMAGE_PIXELS: u64 = 64 * 1024 * 1024;

pub fn crlf_to_lf(s: &str) -> String {
    s.replace("\r\n", "\n")
}

pub fn lf_to_crlf(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / 32);
    let mut prev = '\0';
    for c in s.chars() {
        if c == '\n' && prev != '\r' {
            out.push('\r');
        }
        out.push(c);
        prev = c;
    }
    out
}

const CF_HTML_HEADER: &str = "Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\n\
StartFragment:0000000000\r\nEndFragment:0000000000\r\n";

/// Wraps markup into the `HTML Format` clipboard envelope with byte offsets.
pub fn to_cf_html(html: &str) -> Vec<u8> {
    let start_html = CF_HTML_HEADER.len();
    let prefix = "<html><body>\r\n<!--StartFragment-->";
    let suffix = "<!--EndFragment-->\r\n</body></html>";
    let start_fragment = start_html + prefix.len();
    let end_fragment = start_fragment + html.len();
    let end_html = end_fragment + suffix.len();
    let header = format!(
        "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\n\
         StartFragment:{start_fragment:010}\r\nEndFragment:{end_fragment:010}\r\n"
    );
    let mut out = header.into_bytes();
    out.extend_from_slice(prefix.as_bytes());
    out.extend_from_slice(html.as_bytes());
    out.extend_from_slice(suffix.as_bytes());
    out
}

fn header_offset(header: &str, key: &str) -> Option<usize> {
    header
        .lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
        .and_then(|v| v.trim().parse::<i64>().ok())
        .and_then(|v| usize::try_from(v).ok())
}

/// Extracts the fragment (or the whole document) from an `HTML Format` envelope.
pub fn from_cf_html(data: &[u8]) -> Option<String> {
    let head_len = data.len().min(1024);
    let header = String::from_utf8_lossy(&data[..head_len]);
    let range = |a: &str, b: &str| {
        let (s, e) = (header_offset(&header, a)?, header_offset(&header, b)?);
        (s <= e && e <= data.len()).then_some(s..e)
    };
    let r = range("StartFragment", "EndFragment").or_else(|| range("StartHTML", "EndHTML"))?;
    let text = String::from_utf8_lossy(&data[r]).into_owned();
    Some(text.trim_end_matches('\0').to_owned())
}

/// Width and height from the PNG header, without decoding.
pub fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    const SIG: &[u8] = b"\x89PNG\r\n\x1a\n";
    if png.len() < 24 || !png.starts_with(SIG) || &png[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(png[20..24].try_into().ok()?);
    Some((w, h))
}

fn fits(w: u32, h: u32) -> bool {
    w > 0 && h > 0 && u64::from(w) * u64::from(h) <= MAX_IMAGE_PIXELS
}

/// Decodes a PNG into RGBA8 rows, top to bottom.
pub fn png_to_rgba(png: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let (w, h) = png_dimensions(png)?;
    if !fits(w, h) {
        return None;
    }
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let px = (w * h) as usize;
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf[..px * 4].to_vec(),
        png::ColorType::Rgb => buf[..px * 3]
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|c| [c[0], c[1], c[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf[..px * 2]
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|c| [c[0], c[0], c[0], c[1]])
            .collect(),
        png::ColorType::Grayscale => buf[..px].iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    Some((w, h, rgba))
}

pub fn rgba_to_png(w: u32, h: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    if !fits(w, h) || rgba.len() != (w * h * 4) as usize {
        return None;
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(rgba).ok()?;
    }
    Some(out)
}

const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;

/// `CF_DIB`: BITMAPINFOHEADER + 32-bit BGRA rows, bottom-up.
pub fn png_to_dib(png: &[u8]) -> Option<Vec<u8>> {
    let (w, h, rgba) = png_to_rgba(png)?;
    let mut out = Vec::with_capacity(40 + rgba.len());
    let put32 = |o: &mut Vec<u8>, v: u32| o.extend_from_slice(&v.to_le_bytes());
    put32(&mut out, 40);
    put32(&mut out, w);
    put32(&mut out, h);
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    put32(&mut out, BI_RGB);
    put32(&mut out, w * h * 4);
    for _ in 0..4 {
        put32(&mut out, 0);
    }
    let row = (w * 4) as usize;
    for y in (0..h as usize).rev() {
        for px in rgba[y * row..(y + 1) * row].as_chunks::<4>().0 {
            out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    Some(out)
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// Reads a 24- or 32-bit uncompressed `CF_DIB` into PNG.
pub fn dib_to_png(dib: &[u8]) -> Option<Vec<u8>> {
    let header = le32(dib, 0)? as usize;
    let w = le32(dib, 4)? as i32;
    let h = le32(dib, 8)? as i32;
    let bpp = u16::from_le_bytes(dib.get(14..16)?.try_into().ok()?);
    let compression = le32(dib, 16)?;
    let colors_used = le32(dib, 32).unwrap_or(0) as usize;
    if w <= 0 || h == 0 || !(bpp == 24 || bpp == 32) {
        return None;
    }
    if compression != BI_RGB && !(compression == BI_BITFIELDS && bpp == 32) {
        return None;
    }
    let (w, top_down, ah) = (w as u32, h < 0, h.unsigned_abs());
    if !fits(w, ah) {
        return None;
    }
    let masks = if compression == BI_BITFIELDS && header == 40 {
        12
    } else {
        0
    };
    let start = header + masks + colors_used * 4;
    let stride = ((w as usize * bpp as usize).div_ceil(32)) * 4;
    let pixels = dib.get(start..start + stride * ah as usize)?;
    // Many apps leave alpha at 0 in 32-bit DIBs; treat an all-zero alpha as opaque.
    let alpha_used = bpp == 32 && pixels.as_chunks::<4>().0.iter().any(|p| p[3] != 0);
    let mut rgba = Vec::with_capacity((w * ah * 4) as usize);
    for y in 0..ah as usize {
        let src_y = if top_down { y } else { ah as usize - 1 - y };
        let row = &pixels[src_y * stride..src_y * stride + w as usize * (bpp as usize / 8)];
        for px in row.chunks_exact(bpp as usize / 8) {
            let a = if alpha_used { px[3] } else { 255 };
            rgba.extend_from_slice(&[px[2], px[1], px[0], a]);
        }
    }
    rgba_to_png(w, ah, &rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_ends() {
        assert_eq!(lf_to_crlf("a\nb\r\nc"), "a\r\nb\r\nc");
        assert_eq!(crlf_to_lf("a\r\nb\nc"), "a\nb\nc");
        assert_eq!(crlf_to_lf(&lf_to_crlf("x\n\ny")), "x\n\ny");
    }

    #[test]
    fn cf_html_round_trip_and_offsets() {
        let wrapped = to_cf_html("<b>жирный</b>");
        let text = String::from_utf8(wrapped.clone()).unwrap();
        let start = header_offset(&text, "StartFragment").unwrap();
        let end = header_offset(&text, "EndFragment").unwrap();
        assert_eq!(&wrapped[start..end], "<b>жирный</b>".as_bytes());
        assert_eq!(from_cf_html(&wrapped).as_deref(), Some("<b>жирный</b>"));
        assert_eq!(from_cf_html(b"garbage"), None);
        let bad = b"Version:0.9\r\nStartFragment:900\r\nEndFragment:10\r\n";
        assert_eq!(from_cf_html(bad), None, "reversed offsets");
    }

    fn checker(w: u32, h: u32) -> Vec<u8> {
        (0..w * h)
            .flat_map(|i| {
                let on = (i % w + i / w).is_multiple_of(2);
                if on {
                    [255, 0, 0, 255]
                } else {
                    [0, 0, 255, 128]
                }
            })
            .collect()
    }

    #[test]
    fn png_dib_round_trip() {
        let (w, h) = (5, 3);
        let rgba = checker(w, h);
        let png = rgba_to_png(w, h, &rgba).unwrap();
        assert_eq!(png_dimensions(&png), Some((5, 3)));
        assert_eq!(png_to_rgba(&png).unwrap().2, rgba);
        let dib = png_to_dib(&png).unwrap();
        assert_eq!(dib.len(), 40 + 5 * 3 * 4);
        let back = dib_to_png(&dib).unwrap();
        assert_eq!(png_to_rgba(&back).unwrap().2, rgba);
    }

    #[test]
    fn dib_24_bit_padded_rows_and_opaque_alpha() {
        // 2×1, 24 bit: row is 6 bytes padded to 8.
        let mut dib = Vec::new();
        for v in [40u32, 2, 1] {
            dib.extend_from_slice(&v.to_le_bytes());
        }
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&24u16.to_le_bytes());
        dib.extend_from_slice(&[0; 24]);
        dib.extend_from_slice(&[0, 0, 255, 255, 0, 0, 0, 0]);
        let rgba = png_to_rgba(&dib_to_png(&dib).unwrap()).unwrap().2;
        assert_eq!(rgba, vec![255, 0, 0, 255, 0, 0, 255, 255]);
    }

    #[test]
    fn oversized_and_broken_images_are_refused() {
        let mut fake = rgba_to_png(1, 1, &[0, 0, 0, 0]).unwrap();
        fake[16..20].copy_from_slice(&100_000u32.to_be_bytes());
        fake[20..24].copy_from_slice(&100_000u32.to_be_bytes());
        assert_eq!(png_to_rgba(&fake), None, "pixel limit before decoding");
        assert_eq!(png_dimensions(b"not a png at all, really"), None);
        assert_eq!(dib_to_png(&[0; 10]), None);
        assert_eq!(rgba_to_png(2, 2, &[0; 3]), None);
    }
}
