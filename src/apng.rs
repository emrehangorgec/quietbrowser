// Joins PNG screenshots into one animated PNG without decoding them: the image data of every
// frame is copied as is, which works because all frames come from the same viewport and share one
// header. Browsers play animated PNGs, GitHub included.

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// The chunks of a PNG file as (type, data).
fn chunks(png: &[u8]) -> Option<Vec<([u8; 4], &[u8])>> {
    let mut out = Vec::new();
    let mut i = 8;
    if png.len() < 8 || &png[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    while i + 8 <= png.len() {
        let len = u32::from_be_bytes(png[i..i + 4].try_into().ok()?) as usize;
        let kind: [u8; 4] = png[i + 4..i + 8].try_into().ok()?;
        let data = png.get(i + 8..i + 8 + len)?;
        out.push((kind, data));
        i += 12 + len;
    }
    Some(out)
}

fn put(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// Builds an animated PNG that shows each frame for `delay_ms` (the last one twice as long) and
/// loops. Frames whose size or format differs from the first are left out.
pub fn encode(frames: &[Vec<u8>], delay_ms: u16) -> Result<(Vec<u8>, usize), String> {
    let parsed: Vec<Vec<([u8; 4], &[u8])>> = frames.iter().filter_map(|f| chunks(f)).collect();
    let first = parsed.first().ok_or("no frames were recorded")?;
    let ihdr = first.iter().find(|c| &c.0 == b"IHDR").map(|c| c.1).ok_or("not a PNG")?;
    let usable: Vec<&Vec<([u8; 4], &[u8])>> =
        parsed.iter().filter(|f| f.iter().any(|c| &c.0 == b"IHDR" && c.1 == ihdr)).collect();

    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    put(&mut out, b"IHDR", ihdr);
    let mut actl = Vec::new();
    actl.extend_from_slice(&(usable.len() as u32).to_be_bytes());
    actl.extend_from_slice(&0u32.to_be_bytes()); // loop forever
    put(&mut out, b"acTL", &actl);

    let mut seq = 0u32;
    for (n, frame) in usable.iter().enumerate() {
        let delay = if n + 1 == usable.len() { delay_ms.saturating_mul(2) } else { delay_ms };
        let mut fctl = Vec::with_capacity(26);
        fctl.extend_from_slice(&seq.to_be_bytes());
        fctl.extend_from_slice(&ihdr[0..8]); // width and height
        fctl.extend_from_slice(&[0; 8]); // x and y offset
        fctl.extend_from_slice(&delay.to_be_bytes());
        fctl.extend_from_slice(&1000u16.to_be_bytes());
        fctl.extend_from_slice(&[0, 0]); // dispose none, blend source
        put(&mut out, b"fcTL", &fctl);
        seq += 1;
        for (kind, data) in frame.iter() {
            if kind != b"IDAT" {
                continue;
            }
            if n == 0 {
                put(&mut out, b"IDAT", data);
            } else {
                let mut fdat = Vec::with_capacity(data.len() + 4);
                fdat.extend_from_slice(&seq.to_be_bytes());
                fdat.extend_from_slice(data);
                put(&mut out, b"fdAT", &fdat);
                seq += 1;
            }
        }
    }
    put(&mut out, b"IEND", &[]);
    Ok((out, usable.len()))
}
