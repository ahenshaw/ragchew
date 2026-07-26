//! JS8 modulator and demodulator, parameterized by [`Submode`].
//!
//! JS8: 12000 samples/s, 8-FSK, 79 symbols (three 7-tone Costas sync blocks at
//! symbol offsets 0, 36, 72 plus 58 data symbols), no Gray coding. Submodes
//! differ only in samples-per-symbol (hence tone spacing) and Costas arrays;
//! the payload chain (LDPC/CRC/varicode) is shared. The public no-suffix
//! functions ([`encode_audio`], [`decode_all`], …) default to **Normal**.

use rustfft::num_complex::Complex32;

use crate::js8::ldpc;
use crate::js8::submode::{self, Mode, Submode};

/// Samples per second for JS8.
pub const SAMPLE_RATE: u32 = crate::SAMPLE_RATE;
/// Samples per symbol for **Normal** (kept for display/back-compat).
pub const SAMPLES_PER_SYMBOL: usize = 1920;
/// Tone spacing (Hz) for **Normal**.
pub const TONE_SPACING: f64 = 6.25;
/// Total symbols per transmission (all submodes).
pub const N_SYMBOLS: usize = 79;

/// Costas sync strength noise floor: what [`DecodeResult::sync`] reads when
/// there is nothing but noise at a candidate. Real frames sit well above it.
pub const NOISE_SYNC: f64 = 0.14;

/// Symbol positions carrying data (i.e. not Costas sync), in order.
fn data_positions() -> [usize; 58] {
    let mut p = [0usize; 58];
    let mut k = 0;
    for si in 0..N_SYMBOLS {
        let is_costas = si < 7 || (si >= 36 && si < 36 + 7) || si >= 72;
        if !is_costas {
            p[k] = si;
            k += 1;
        }
    }
    p
}

/// Map a 174-bit codeword to the 79 transmitted tone values for `sm`, inserting
/// the three Costas sync blocks.
pub fn recode_sm(codeword: &[u8; 174], sm: &Submode) -> [u8; 79] {
    let mut out = [0u8; 79];
    let mut ci = 0usize;
    for si in 0..N_SYMBOLS {
        let is_costas = si < 7 || (36..43).contains(&si) || si >= 72;
        if is_costas {
            out[si] = sm.costas_at(si);
        } else {
            out[si] = (codeword[ci] << 2) | (codeword[ci + 1] << 1) | codeword[ci + 2];
            ci += 3;
        }
    }
    out
}

/// Normal-mode [`recode_sm`].
pub fn recode(codeword: &[u8; 174]) -> [u8; 79] {
    recode_sm(codeword, &submode::NORMAL)
}

/// Continuous-phase 8-FSK synthesis of `symbols` at carrier `base_hz` for `sm`.
pub fn synthesize_sm(symbols: &[u8], base_hz: f64, sm: &Submode) -> Vec<f32> {
    let mut v = Vec::with_capacity(symbols.len() * sm.nsps);
    let mut phase = 0.0f64;
    let dt = 1.0 / SAMPLE_RATE as f64;
    let spacing = sm.spacing();
    for &sym in symbols {
        let hz = base_hz + sym as f64 * spacing;
        let dphi = 2.0 * std::f64::consts::PI * dt * hz;
        for _ in 0..sm.nsps {
            v.push(phase.cos() as f32);
            phase += dphi;
        }
    }
    v
}

/// Normal-mode [`synthesize_sm`].
pub fn synthesize(symbols: &[u8], base_hz: f64) -> Vec<f32> {
    synthesize_sm(symbols, base_hz, &submode::NORMAL)
}

/// Encode 87 message bits to an audio burst at `base_hz` for `sm` (no silence).
pub fn encode_audio_sm(a87: &[u8; 87], base_hz: f64, sm: &Submode) -> Vec<f32> {
    let cw = ldpc::encode(a87);
    let syms = recode_sm(&cw, sm);
    synthesize_sm(&syms, base_hz, sm)
}

/// Normal-mode [`encode_audio_sm`].
pub fn encode_audio(a87: &[u8; 87], base_hz: f64) -> Vec<f32> {
    encode_audio_sm(a87, base_hz, &submode::NORMAL)
}

// ---- demodulation ----

/// Forward FFT of one `n`-sample symbol window at `start` (zero-padded), after
/// mixing down by `delta_hz` so a between-bins carrier lands on an integer bin.
fn symbol_fft_shift(samples: &[f32], start: usize, delta_hz: f64, n: usize) -> Vec<Complex32> {
    crate::dsp::fft_window(samples, start, delta_hz, SAMPLE_RATE, n, None)
}

/// Tone magnitudes (8 tones) for every symbol, for carrier `bin0 + delta/spacing`.
fn tone_mags_frac(samples: &[f32], bin0: usize, delta: f64, off: usize, sm: &Submode) -> Vec<[f64; 8]> {
    let mut out = vec![[0.0f64; 8]; N_SYMBOLS];
    for si in 0..N_SYMBOLS {
        let spec = symbol_fft_shift(samples, off + si * sm.nsps, delta, sm.nsps);
        for t in 0..8 {
            out[si][t] = spec[bin0 + t].norm() as f64;
        }
    }
    out
}

/// Costas sync strength (sig/noise) for a candidate (bin0 + delta/spacing, off).
fn costas_strength_frac(samples: &[f32], bin0: usize, delta: f64, off: usize, sm: &Submode) -> f64 {
    let mut sig = 0.0f64;
    let mut noise = 0.0f64;
    for (bi, &block) in [0usize, 36, 72].iter().enumerate() {
        for s in 0..7 {
            let spec = symbol_fft_shift(samples, off + (block + s) * sm.nsps, delta, sm.nsps);
            for t in 0..8 {
                let mag = spec[bin0 + t].norm() as f64;
                if t == sm.costas[bi][s] as usize {
                    sig += mag;
                } else {
                    noise += mag;
                }
            }
        }
    }
    if noise > 0.0 {
        sig / noise
    } else {
        0.0
    }
}

fn costas_strength(samples: &[f32], bin0: usize, off: usize, sm: &Submode) -> f64 {
    costas_strength_frac(samples, bin0, 0.0, off, sm)
}

/// Convert per-symbol tone magnitudes into 174 soft LLRs
/// (`log(P(bit=0)/P(bit=1))`, positive => 0). Mode-independent.
fn soft_llrs(mags: &[[f64; 8]]) -> [f64; 174] {
    let positions = data_positions();
    let mut llr = [0.0f64; 174];
    for (d, &si) in positions.iter().enumerate() {
        let m = &mags[si];
        let mean: f64 = m.iter().sum::<f64>() / 8.0;
        let scale = if mean > 0.0 { 1.0 / mean } else { 1.0 };
        for t in 0..3 {
            let weight = 2usize.pow((2 - t) as u32);
            let mut m0 = f64::MIN;
            let mut m1 = f64::MIN;
            for tone in 0..8usize {
                let v = m[tone] * scale;
                if (tone / weight) & 1 == 0 {
                    m0 = m0.max(v);
                } else {
                    m1 = m1.max(v);
                }
            }
            llr[d * 3 + t] = 4.0 * (m0 - m1);
        }
    }
    llr
}

/// Number of LDPC parity checks satisfied by the *hard* decision of these tone
/// magnitudes — a cheap noise filter (random tones satisfy ~43/87).
fn hard_parity_score(mags: &[[f64; 8]]) -> u32 {
    let positions = data_positions();
    let mut cw = [0u8; 174];
    for (d, &si) in positions.iter().enumerate() {
        let m = &mags[si];
        let mut best = 0usize;
        for t in 1..8 {
            if m[t] > m[best] {
                best = t;
            }
        }
        cw[3 * d] = ((best >> 2) & 1) as u8;
        cw[3 * d + 1] = ((best >> 1) & 1) as u8;
        cw[3 * d + 2] = (best & 1) as u8;
    }
    ldpc::check(&cw)
}

/// Outcome of decoding one candidate signal.
pub struct DecodeResult {
    /// Submode this signal was decoded as.
    pub mode: Mode,
    /// Carrier frequency (Hz).
    pub hz: f64,
    /// Sample offset of symbol 0.
    pub offset: usize,
    /// Costas sync strength.
    pub sync: f64,
    /// Decoded 87 message bits.
    pub a87: [u8; 87],
    /// Whether the LDPC decode fully converged.
    pub ldpc_ok: bool,
    /// LDPC parity-check score (87 = full).
    pub ldpc_score: u32,
}

/// Demodulate + LDPC-decode at carrier `bin0 + delta/spacing` and offset `off`.
fn decode_at_freq(
    samples: &[f32],
    bin0: usize,
    delta: f64,
    off: usize,
    iters: usize,
    sm: &Submode,
) -> DecodeResult {
    let mags = tone_mags_frac(samples, bin0, delta, off, sm);
    let llr = soft_llrs(&mags);
    let dec = ldpc::decode(&llr, iters);
    DecodeResult {
        mode: sm.mode,
        hz: bin0 as f64 * sm.spacing() + delta,
        offset: off,
        sync: costas_strength_frac(samples, bin0, delta, off, sm),
        a87: dec.message_bits(),
        ldpc_ok: dec.ok(),
        ldpc_score: dec.score,
    }
}

fn decode_bins(samples: &[f32], bin0: usize, off: usize, iters: usize, sm: &Submode) -> DecodeResult {
    decode_at_freq(samples, bin0, 0.0, off, iters, sm)
}

/// Decode around a coarse (bin0, off0), searching a fractional carrier and the
/// symbol-0 offset and returning the first CRC-passing candidate (the real
/// gate). Falls back to the best-scoring result.
fn decode_search(
    samples: &[f32],
    bin0: usize,
    off0: usize,
    off_span: isize,
    off_step: isize,
    iters: usize,
    sm: &Submode,
) -> DecodeResult {
    let need = N_SYMBOLS * sm.nsps;
    let fits = |off: usize| off + need <= samples.len();
    let delta_max = sm.spacing() / 2.0;

    // 1) coarse fractional-carrier estimate
    let mut best_delta = 0.0f64;
    let mut best_ds = f64::MIN;
    let mut df = -delta_max;
    while df <= delta_max + 1e-9 {
        let s = costas_strength_frac(samples, bin0, df, off0, sm);
        if s > best_ds {
            best_ds = s;
            best_delta = df;
        }
        df += sm.spacing() / 8.0;
    }

    // 2) coarse offset estimate
    let mut best_off = off0;
    let mut best_os = costas_strength_frac(samples, bin0, best_delta, off0, sm);
    let mut d = -off_span;
    while d <= off_span {
        let off = off0 as isize + d;
        if off >= 0 && fits(off as usize) {
            let s = costas_strength_frac(samples, bin0, best_delta, off as usize, sm);
            if s > best_os {
                best_os = s;
                best_off = off as usize;
            }
        }
        d += 24;
    }

    // Early out: no plausible sync here (empty slot) — skip the expensive grid.
    // Noise Costas strength sits near 0.14; real frames are >~0.3.
    if best_os < 0.20 {
        return DecodeResult {
            mode: sm.mode,
            hz: bin0 as f64 * sm.spacing() + best_delta,
            offset: best_off,
            sync: best_os,
            a87: [0u8; 87],
            ldpc_ok: false,
            ldpc_score: 0,
        };
    }

    // 3) CRC-gated grid (offset up to ~half a symbol from the Costas peak).
    // Each grid point's tone magnitudes are hard-parity gated *before* running
    // belief-propagation, so wrong offsets/deltas cost only the demod, not LDPC.
    let fine = sm.nsps as isize / 2;
    let deltas = [-0.25, 0.0, 0.25].map(|f| f * sm.spacing() / 6.25);
    let mut best: Option<DecodeResult> = None;
    let mut d = -fine;
    while d <= fine {
        let off = best_off as isize + d;
        if off >= 0 && fits(off as usize) {
            let off = off as usize;
            for dd in deltas {
                let delta = (best_delta + dd).clamp(-delta_max, delta_max);
                let mags = tone_mags_frac(samples, bin0, delta, off, sm);
                if hard_parity_score(&mags) < 44 {
                    continue;
                }
                let dec = ldpc::decode(&soft_llrs(&mags), iters);
                let a87 = dec.message_bits();
                let r = DecodeResult {
                    mode: sm.mode,
                    hz: bin0 as f64 * sm.spacing() + delta,
                    offset: off,
                    sync: 0.0,
                    a87,
                    ldpc_ok: dec.ok(),
                    ldpc_score: dec.score,
                };
                if r.ldpc_ok && crate::js8::message::crc_ok(&r.a87) {
                    return DecodeResult { sync: costas_strength_frac(samples, bin0, delta, off, sm), ..r };
                }
                if best.as_ref().map_or(true, |b| r.ldpc_score > b.ldpc_score) {
                    best = Some(r);
                }
            }
        }
        d += off_step;
    }
    best.unwrap_or_else(|| decode_at_freq(samples, bin0, best_delta, best_off, iters, sm))
}

fn refine_offset(samples: &[f32], bin0: usize, off0: usize, sm: &Submode) -> (usize, f64) {
    let span = sm.nsps as isize / 2;
    let need = N_SYMBOLS * sm.nsps;
    let mut best = (off0, costas_strength(samples, bin0, off0, sm));
    let mut d = -span;
    while d <= span {
        let off = off0 as isize + d;
        if off >= 0 && (off as usize) + need <= samples.len() {
            let s = costas_strength(samples, bin0, off as usize, sm);
            if s > best.1 {
                best = (off as usize, s);
            }
        }
        d += 24;
    }
    best
}

fn find_sync(
    samples: &[f32],
    bin_lo: usize,
    bin_hi: usize,
    off_lo: usize,
    off_hi: usize,
    off_step: usize,
    sm: &Submode,
) -> Option<(usize, usize, f64)> {
    let need = N_SYMBOLS * sm.nsps;
    let mut best: Option<(usize, usize, f64)> = None;
    let mut off = off_lo;
    while off <= off_hi {
        if off + need <= samples.len() {
            for bin0 in bin_lo..=bin_hi {
                let s = costas_strength(samples, bin0, off, sm);
                if best.map_or(true, |(_, _, bs)| s > bs) {
                    best = Some((bin0, off, s));
                }
            }
        }
        off += off_step.max(1);
    }
    best
}

/// Decode at an exact carrier `base_hz` (fractional Hz allowed) and offset,
/// with no sync refinement, as Normal.
pub fn decode_exact(samples: &[f32], base_hz: f64, offset: usize, iters: usize) -> DecodeResult {
    let sm = &submode::NORMAL;
    let bin0 = (base_hz / sm.spacing()).round() as usize;
    let delta = base_hz - bin0 as f64 * sm.spacing();
    decode_at_freq(samples, bin0, delta, offset, iters, sm)
}

/// Decode a single Normal JS8 signal near a known carrier and start offset.
pub fn decode_near(
    samples: &[f32],
    base_hz: f64,
    start_sample: usize,
    search_symbols: usize,
    iters: usize,
) -> Option<DecodeResult> {
    let sm = &submode::NORMAL;
    let center_bin = (base_hz / sm.spacing()).round() as usize;
    let bin_lo = center_bin.saturating_sub(2);
    let bin_hi = center_bin + 2;
    let win = search_symbols * sm.nsps;
    let off_lo = start_sample.saturating_sub(win);
    let off_hi = start_sample + win;
    let (bin0, off, _s) = find_sync(samples, bin_lo, bin_hi, off_lo, off_hi, sm.nsps / 8, sm)?;
    let (off, _s) = refine_offset(samples, bin0, off, sm);
    Some(decode_bins(samples, bin0, off, iters, sm))
}

// ---- wideband / full-timeline scan via a spectrogram ----

/// Sub-symbol steps used by the sync spectrogram (hop = symbol / STRIDE).
const STRIDE: usize = 8;

/// Magnitude spectrogram for one submode: `mags[frame][bin - bin_lo]`, frame `f`
/// starting at sample `f * hop`, `hop = nsps / STRIDE`.
struct Spectro {
    sm: Submode,
    bin_lo: usize,
    n_bins: usize,
    hop: usize,
    mags: Vec<f32>,
    n_frames: usize,
}

impl Spectro {
    fn compute(samples: &[f32], bin_lo: usize, bin_hi: usize, sm: Submode) -> Spectro {
        let hop = sm.nsps / STRIDE;
        let n_bins = bin_hi - bin_lo + 1;
        let n_frames = if samples.len() >= sm.nsps {
            (samples.len() - sm.nsps) / hop + 1
        } else {
            0
        };
        let mut mags = vec![0.0f32; n_frames * n_bins];
        for f in 0..n_frames {
            let spec = symbol_fft_shift(samples, f * hop, 0.0, sm.nsps);
            let row = &mut mags[f * n_bins..(f + 1) * n_bins];
            for (j, m) in row.iter_mut().enumerate() {
                *m = spec[bin_lo + j].norm();
            }
        }
        Spectro { sm, bin_lo, n_bins, hop, mags, n_frames }
    }

    #[inline]
    fn mag(&self, frame: usize, bin: usize) -> f32 {
        self.mags[frame * self.n_bins + (bin - self.bin_lo)]
    }

    fn symbol_mags(&self, frame: usize, bin0: usize) -> Option<Vec<[f64; 8]>> {
        if frame + (N_SYMBOLS - 1) * STRIDE >= self.n_frames || bin0 + 7 >= self.bin_lo + self.n_bins {
            return None;
        }
        let mut out = vec![[0.0f64; 8]; N_SYMBOLS];
        for si in 0..N_SYMBOLS {
            let fr = frame + si * STRIDE;
            for t in 0..8 {
                out[si][t] = self.mag(fr, bin0 + t) as f64;
            }
        }
        Some(out)
    }

    fn strength(&self, frame: usize, bin0: usize) -> f32 {
        let mut sig = 0.0f32;
        let mut noise = 0.0f32;
        for (bi, &block) in [0usize, 36, 72].iter().enumerate() {
            for s in 0..7 {
                let fr = frame + (block + s) * STRIDE;
                for t in 0..8 {
                    let m = self.mag(fr, bin0 + t);
                    if t == self.sm.costas[bi][s] as usize {
                        sig += m;
                    } else {
                        noise += m;
                    }
                }
            }
        }
        if noise > 0.0 {
            sig / noise
        } else {
            0.0
        }
    }
}

/// Decode every frame of submode `sm` found in `samples` within `hz_lo..hz_hi`.
pub fn decode_all_sm(
    samples: &[f32],
    hz_lo: f64,
    hz_hi: f64,
    iters: usize,
    sm: &Submode,
) -> Vec<DecodeResult> {
    let spacing = sm.spacing();
    let bin_lo = ((hz_lo / spacing).floor() as usize).max(1);
    let bin_hi = (hz_hi / spacing).ceil() as usize + 7;
    let spec = Spectro::compute(samples, bin_lo, bin_hi + 1, *sm);
    if spec.n_frames == 0 {
        return Vec::new();
    }
    let last_frame = spec.n_frames.saturating_sub((N_SYMBOLS - 1) * STRIDE + 1);

    let mut cands: Vec<(f32, usize, usize)> = Vec::new();
    for frame in 0..last_frame {
        for bin0 in bin_lo..=bin_hi {
            cands.push((spec.strength(frame, bin0), frame, bin0));
        }
    }
    cands.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());

    let mut out: Vec<DecodeResult> = Vec::new();
    let overlaps = |out: &[DecodeResult], bin0: usize, off0: usize| {
        out.iter().any(|r| {
            let df = (r.hz / spacing).round() as i64 - bin0 as i64;
            let dt = (r.offset as i64 - off0 as i64).abs();
            df.abs() <= 1 && dt < 4 * SAMPLE_RATE as i64
        })
    };

    // Pass 1: fast spectrogram decode of the strongest peaks (hard-parity gated).
    const GATE: u32 = 48;
    let mut examined = 0usize;
    let mut ldpc_tries = 0usize;
    for (s, frame, bin0) in &cands {
        examined += 1;
        if examined > 20000 || ldpc_tries > 400 {
            break;
        }
        let off0 = frame * spec.hop;
        if overlaps(&out, *bin0, off0) {
            continue;
        }
        let mags = match spec.symbol_mags(*frame, *bin0) {
            Some(m) => m,
            None => continue,
        };
        if hard_parity_score(&mags) < GATE {
            continue;
        }
        ldpc_tries += 1;
        let dec = ldpc::decode(&soft_llrs(&mags), iters);
        if dec.ok() {
            let a87 = dec.message_bits();
            if crate::js8::message::crc_ok(&a87) {
                out.push(DecodeResult {
                    mode: sm.mode,
                    hz: *bin0 as f64 * spacing,
                    offset: off0,
                    sync: *s as f64,
                    a87,
                    ldpc_ok: true,
                    ldpc_score: dec.score,
                });
            }
        }
    }

    // Pass 2: fill predicted empty slots on active carriers (frames repeat on a
    // fixed `period_s` grid).
    if out.len() >= 2 {
        let period = sm.period_s as i64 * SAMPLE_RATE as i64;
        let mut phases: Vec<i64> = out.iter().map(|r| r.offset as i64 % period).collect();
        phases.sort_unstable();
        let phase = phases[phases.len() / 2];

        let mut active: Vec<usize> = out
            .iter()
            .flat_map(|r| {
                let b = (r.hz / spacing).round() as usize;
                [b.saturating_sub(1), b, b + 1]
            })
            .collect();
        active.sort_unstable();
        active.dedup();

        let off_span = (sm.nsps / 2) as isize;
        let coarse_win = (1.2 * SAMPLE_RATE as f64) as i64;
        let need = (N_SYMBOLS * sm.nsps) as i64;
        let kmax = ((samples.len() as i64 - need - phase) / period).max(0);
        // bound the (expensive) CRC-gated fine searches for long recordings
        let mut searches = 0usize;
        'pass2: for k in 0..=kmax {
            let pred = phase + k * period;
            if overlaps(&out, active[active.len() / 2], pred as usize) {
                continue;
            }
            for &bin0 in &active {
                if bin0 < bin_lo || bin0 > bin_hi || overlaps(&out, bin0, pred as usize) {
                    continue;
                }
                let f_lo = ((pred - coarse_win).max(0) as usize) / spec.hop;
                let f_hi =
                    (((pred + coarse_win) as usize) / spec.hop).min(last_frame.saturating_sub(1));
                let mut best: Option<(f32, usize)> = None;
                for f in f_lo..=f_hi {
                    let s = spec.strength(f, bin0);
                    if best.map_or(true, |(bs, _)| s > bs) {
                        best = Some((s, f));
                    }
                }
                if let Some((_s, f)) = best {
                    // Cheap pre-gate from the spectrogram (no FFTs): skip slots
                    // with no plausible frame so empty cycles cost ~nothing. Use
                    // a looser gate than pass 1 so marginal frames still qualify.
                    match spec.symbol_mags(f, bin0) {
                        Some(m) if hard_parity_score(&m) >= 44 => {}
                        _ => continue,
                    }
                    searches += 1;
                    if searches > 200 {
                        break 'pass2;
                    }
                    let r = decode_search(samples, bin0, f * spec.hop, off_span, 12, iters, sm);
                    if r.ldpc_ok && crate::js8::message::crc_ok(&r.a87) {
                        out.push(r);
                        break;
                    }
                }
            }
        }
    }

    out.sort_by_key(|r| r.offset);
    out
}

/// Decode every **Normal** frame in `samples` within `hz_lo..hz_hi`.
pub fn decode_all(samples: &[f32], hz_lo: f64, hz_hi: f64, iters: usize) -> Vec<DecodeResult> {
    decode_all_sm(samples, hz_lo, hz_hi, iters, &submode::NORMAL)
}

/// Decode every frame of **every** submode (Slow/Normal/Fast/Turbo), auto-
/// detecting the mode of each signal. Results are tagged with their [`Mode`]
/// and returned in time order.
pub fn decode_all_multi(samples: &[f32], hz_lo: f64, hz_hi: f64, iters: usize) -> Vec<DecodeResult> {
    let mut out = Vec::new();
    for sm in submode::ALL.iter() {
        out.extend(decode_all_sm(samples, hz_lo, hz_hi, iters, sm));
    }
    out.sort_by_key(|r| r.offset);
    out
}

/// Scan a frequency band for Normal signals near `start_sample` (single cycle).
pub fn decode_scan(
    samples: &[f32],
    hz_lo: f64,
    hz_hi: f64,
    start_sample: usize,
    off_search_symbols: usize,
    iters: usize,
) -> Vec<DecodeResult> {
    let win = (off_search_symbols * SAMPLES_PER_SYMBOL) as i64;
    let lo = (start_sample as i64 - win).max(0) as usize;
    let hi = start_sample + off_search_symbols * SAMPLES_PER_SYMBOL;
    decode_all(samples, hz_lo, hz_hi, iters)
        .into_iter()
        .filter(|r| r.offset >= lo && r.offset <= hi)
        .collect()
}
