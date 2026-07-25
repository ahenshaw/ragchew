//! Minimal 16-bit PCM mono WAV read/write (enough for JS8 audio I/O and tests).

use std::io::{self, Read, Write};

/// Read a mono 16-bit PCM WAV file into normalized `f32` samples in `[-1, 1]`,
/// returning `(samples, sample_rate)`. Only the subset of WAV needed here is
/// supported.
pub fn read(path: &str) -> io::Result<(Vec<f32>, u32)> {
    let mut buf = Vec::new();
    std::fs::File::open(path)?.read_to_end(&mut buf)?;
    if buf.len() < 44 || &buf[0..4] != b"RIFF" || &buf[8..12] != b"WAVE" {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a RIFF/WAVE file"));
    }
    let mut rate = 0u32;
    let mut bits = 16u16;
    let mut pos = 12usize;
    let mut data: Option<(usize, usize)> = None;
    while pos + 8 <= buf.len() {
        let id = &buf[pos..pos + 4];
        let sz = u32::from_le_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]])
            as usize;
        let body = pos + 8;
        if id == b"fmt " {
            rate = u32::from_le_bytes([buf[body + 4], buf[body + 5], buf[body + 6], buf[body + 7]]);
            bits = u16::from_le_bytes([buf[body + 14], buf[body + 15]]);
        } else if id == b"data" {
            data = Some((body, sz.min(buf.len() - body)));
        }
        pos = body + sz + (sz & 1);
    }
    let (start, len) = data.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no data chunk"))?;
    if bits != 16 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "only 16-bit PCM supported"));
    }
    let mut samples = Vec::with_capacity(len / 2);
    let mut i = start;
    while i + 1 < start + len {
        let s = i16::from_le_bytes([buf[i], buf[i + 1]]);
        samples.push(s as f32 / 32768.0);
        i += 2;
    }
    Ok((samples, rate))
}

/// Write normalized `f32` samples as a mono 16-bit PCM WAV file.
pub fn write(path: &str, samples: &[f32], rate: u32) -> io::Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let data_len = (samples.len() * 2) as u32;
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_len).to_le_bytes())?;
    f.write_all(b"WAVE")?;
    f.write_all(b"fmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&1u16.to_le_bytes())?; // mono
    f.write_all(&rate.to_le_bytes())?;
    f.write_all(&(rate * 2).to_le_bytes())?; // byte rate
    f.write_all(&2u16.to_le_bytes())?; // block align
    f.write_all(&16u16.to_le_bytes())?; // bits
    f.write_all(b"data")?;
    f.write_all(&data_len.to_le_bytes())?;
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        f.write_all(&v.to_le_bytes())?;
    }
    Ok(())
}
