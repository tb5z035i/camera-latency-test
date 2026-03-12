use crate::analysis::MeasurementMethod;

#[derive(Debug, Clone, Copy)]
pub struct StimulusState {
    pub transition_id: u64,
    pub colors: [[u8; 3]; 4],
}

pub fn state_for(method: MeasurementMethod, transition_id: u64) -> StimulusState {
    match method {
        MeasurementMethod::LumaStep => {
            let v = if transition_id % 2 == 0 { 16 } else { 240 };
            StimulusState {
                transition_id,
                colors: [[v, v, v]; 4],
            }
        }
        MeasurementMethod::QuadCode => {
            let code = (transition_id % 16) as u8;
            let mut colors = [[0, 0, 0]; 4];
            for (i, c) in colors.iter_mut().enumerate() {
                let bit = (code >> i) & 1;
                *c = if bit == 1 {
                    [240, 240, 240]
                } else {
                    [16, 16, 16]
                };
            }
            StimulusState {
                transition_id,
                colors,
            }
        }
    }
}

pub fn decode_quad_code(lumas: [f32; 4], threshold: f32, previous_id: Option<u64>) -> Option<u64> {
    let mut code = 0u8;
    for (i, luma) in lumas.iter().enumerate() {
        if *luma > threshold {
            code |= 1 << i;
        }
    }

    match previous_id {
        None => Some(code as u64),
        Some(prev) => {
            let base = prev & !0xF;
            let candidates = [base.wrapping_sub(16), base, base + 16, base + 32];
            candidates
                .into_iter()
                .map(|block| block + code as u64)
                .filter(|id| *id > prev && *id <= prev + 32)
                .min()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quad_code_decodes_forward_sequence() {
        let threshold = 128.0;
        let mut prev = None;

        for expected in 0..20u64 {
            let code = (expected % 16) as u8;
            let lumas = [
                if code & 1 == 1 { 240.0 } else { 16.0 },
                if code & 2 == 2 { 240.0 } else { 16.0 },
                if code & 4 == 4 { 240.0 } else { 16.0 },
                if code & 8 == 8 { 240.0 } else { 16.0 },
            ];
            let id = decode_quad_code(lumas, threshold, prev);
            assert_eq!(id, Some(expected));
            prev = id;
        }
    }
}
