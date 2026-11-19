pub(crate) struct Resampler {
    step: f64,
    offset: u64,
    history: Vec<i16>,
    next_out: u64,
}

impl Resampler {
    pub(crate) fn new(in_rate: u32, out_rate: u32) -> Self {
        Self { step: in_rate as f64 / out_rate as f64, offset: 0, history: Vec::new(), next_out: 0 }
    }

    pub(crate) fn push(&mut self, input: &[i16]) -> Vec<i16> {
        if (self.step - 1.0).abs() < f64::EPSILON {
            return input.to_vec();
        }
        self.history.extend_from_slice(input);
        let end = self.offset + self.history.len() as u64;
        let mut out = Vec::with_capacity((input.len() as f64 / self.step) as usize + 2);
        loop {
            let pos = self.next_out as f64 * self.step;
            let i = pos.floor() as u64;
            if i + 1 >= end {
                break;
            }
            let frac = pos - i as f64;
            let a = self.history[(i - self.offset) as usize] as f64;
            let b = self.history[(i + 1 - self.offset) as usize] as f64;
            out.push((a + (b - a) * frac).round() as i16);
            self.next_out += 1;
        }
        let keep_from = (self.next_out as f64 * self.step).floor() as u64;
        if keep_from > self.offset {
            let drop = (keep_from - self.offset) as usize;
            self.history.drain(..drop.min(self.history.len()));
            self.offset += drop as u64;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_produces_three_halves_and_is_chunk_invariant() {
        let input: Vec<i16> = (0..16_000).map(|i| ((i % 200) * 100) as i16).collect();
        let mut whole = Resampler::new(16_000, 24_000);
        let a = whole.push(&input);
        let mut chunked = Resampler::new(16_000, 24_000);
        let b: Vec<i16> = input.chunks(137).flat_map(|c| chunked.push(c)).collect();
        assert_eq!(a, b);
        assert!((a.len() as i64 - 24_000).abs() <= 2, "got {}", a.len());
        assert_eq!(a[0], input[0]);
        assert_eq!(a[3], input[2]);
        assert_eq!(a[300], input[200]);
    }
}
