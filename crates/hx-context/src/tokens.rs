//! Token estimation: bytes / 4, scaled by a ratio calibrated against provider usage.

/// Weight of the newest sample in the calibration average.
const EMA_ALPHA: f64 = 0.3;
const MIN_RATIO: f64 = 0.5;
const MAX_RATIO: f64 = 2.0;

#[derive(Debug, Clone, PartialEq)]
pub struct TokenEstimator {
    ratio: f64,
}

impl Default for TokenEstimator {
    fn default() -> Self {
        Self { ratio: 1.0 }
    }
}

impl TokenEstimator {
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// `ceil(bytes / 4 * ratio)`.
    pub fn estimate(&self, s: &str) -> u32 {
        (s.len() as f64 / 4.0 * self.ratio).ceil() as u32
    }

    /// [`calibrate`](Self::calibrate) for totals that include `flat` tokens of fixed
    /// estimates (images) which the ratio does not scale: the sample is the text part only.
    pub fn calibrate_excluding(&mut self, estimated: u32, actual: u64, flat: u32) {
        self.calibrate(
            estimated.saturating_sub(flat),
            actual.saturating_sub(u64::from(flat)),
        );
    }

    /// Fold one observation into the ratio: `estimated` is a count this estimator produced
    /// (so it already includes the current ratio) and `actual` the provider's count for the
    /// same text. The ratio moves toward `actual / unscaled estimate`, an exponential moving
    /// average clamped to 0.5-2.0; a zero estimate carries no information and is ignored.
    pub fn calibrate(&mut self, estimated: u32, actual: u64) {
        if estimated == 0 {
            return;
        }
        // Observed ratio of our count to the provider's, relative to the unscaled estimate.
        let sample = actual as f64 / (estimated as f64 / self.ratio);
        self.ratio =
            (self.ratio * (1.0 - EMA_ALPHA) + sample * EMA_ALPHA).clamp(MIN_RATIO, MAX_RATIO);
    }
}
