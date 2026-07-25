//! JS8 submodes. All submodes share the payload chain (8-FSK, 79 symbols,
//! LDPC(174,87), CRC-12, varicode); they differ only in symbol timing, tone
//! spacing, and Costas sync arrays. Parameters from JS8Call's `js8*_params.f90`
//! and Costas arrays from `syncjs8.f90` / `genjs8.f90`.

use crate::modem::{N_SYMBOLS, SAMPLE_RATE};

/// The four standard JS8 submodes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Slow,
    Normal,
    Fast,
    Turbo,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Slow => "Slow",
            Mode::Normal => "Normal",
            Mode::Fast => "Fast",
            Mode::Turbo => "Turbo",
        }
    }
    /// One-letter tag (JS8Call's A/B/C/E).
    pub fn tag(self) -> char {
        match self {
            Mode::Slow => 'E',
            Mode::Normal => 'A',
            Mode::Fast => 'B',
            Mode::Turbo => 'C',
        }
    }
}

/// Timing/sync parameters for a submode (at 12 kHz).
#[derive(Clone, Copy)]
pub struct Submode {
    pub mode: Mode,
    /// Samples per symbol.
    pub nsps: usize,
    /// Transmit/receive period (seconds) the mode aligns to on the UTC grid.
    pub period_s: u32,
    /// The three 7-tone Costas sync arrays (begin / middle / end blocks).
    pub costas: [[u8; 7]; 3],
}

// NCOSTAS=1 (Normal): the original array in all three blocks.
const COSTAS_A: [[u8; 7]; 3] =
    [[4, 2, 5, 6, 1, 3, 0], [4, 2, 5, 6, 1, 3, 0], [4, 2, 5, 6, 1, 3, 0]];
// NCOSTAS=2 (Fast/Turbo/Slow): three distinct symmetrical arrays.
const COSTAS_B: [[u8; 7]; 3] =
    [[0, 6, 2, 3, 5, 4, 1], [1, 5, 0, 2, 3, 6, 4], [2, 5, 0, 6, 4, 1, 3]];

pub const SLOW: Submode = Submode { mode: Mode::Slow, nsps: 3840, period_s: 30, costas: COSTAS_B };
pub const NORMAL: Submode = Submode { mode: Mode::Normal, nsps: 1920, period_s: 15, costas: COSTAS_A };
pub const FAST: Submode = Submode { mode: Mode::Fast, nsps: 1200, period_s: 10, costas: COSTAS_B };
pub const TURBO: Submode = Submode { mode: Mode::Turbo, nsps: 600, period_s: 6, costas: COSTAS_B };

/// All standard submodes.
pub const ALL: [Submode; 4] = [NORMAL, FAST, TURBO, SLOW];

/// The [`Submode`] parameters for a [`Mode`].
pub fn of(mode: Mode) -> Submode {
    match mode {
        Mode::Slow => SLOW,
        Mode::Normal => NORMAL,
        Mode::Fast => FAST,
        Mode::Turbo => TURBO,
    }
}

impl Submode {
    /// Tone spacing in Hz (`12000 / nsps`, also the FFT bin width for this mode).
    pub fn spacing(&self) -> f64 {
        SAMPLE_RATE as f64 / self.nsps as f64
    }
    /// Samples in a full 79-symbol frame.
    pub fn frame_samples(&self) -> usize {
        N_SYMBOLS * self.nsps
    }
    /// Duration of a full frame (seconds).
    pub fn frame_secs(&self) -> f64 {
        self.frame_samples() as f64 / SAMPLE_RATE as f64
    }
    /// Costas tone expected at sync-symbol position `si` (must be in a sync block).
    pub fn costas_at(&self, si: usize) -> u8 {
        if si < 7 {
            self.costas[0][si]
        } else if (36..43).contains(&si) {
            self.costas[1][si - 36]
        } else {
            self.costas[2][si - 72]
        }
    }
}
