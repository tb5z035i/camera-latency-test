use crate::analysis::MeasurementMethod;

#[derive(Debug, Clone, Copy)]
pub struct StimulusState {
    pub transition_id: u64,
    pub colors: [[u8; 3]; 4],
}

pub fn state_for(method: MeasurementMethod, transition_id: u64, state_space: u64) -> StimulusState {
    match method {
        MeasurementMethod::LumaStep => {
            let v = if transition_id % 2 == 0 { 16 } else { 240 };
            StimulusState {
                transition_id,
                colors: [[v, v, v]; 4],
            }
        }
        MeasurementMethod::QuadCode => {
            let code = (transition_id % state_space.max(2)) as u16;
            let mut colors = [[0, 0, 0]; 4];
            for (i, c) in colors.iter_mut().enumerate() {
                let nibble = ((code >> (i * 4)) & 0xF) as u8;
                let v = 16 + nibble.saturating_mul(14);
                *c = [v, v, v];
            }
            StimulusState {
                transition_id,
                colors,
            }
        }
    }
}

pub fn cyclic_forward_distance(from: u64, to: u64, state_space: u64) -> u64 {
    let n = state_space.max(2);
    if to >= from {
        to - from
    } else {
        n - from + to
    }
}

pub fn decode_quad_code(
    lumas: [f32; 4],
    previous_id: Option<u64>,
    state_space: u64,
    max_forward_jump_codes: u64,
) -> Option<u64> {
    let state_space = state_space.max(2);
    let max_forward_jump_codes = max_forward_jump_codes.max(1).min(state_space - 1);
    let mut code = 0u64;
    for (i, luma) in lumas.iter().enumerate() {
        let normalized = ((*luma - 16.0) / 14.0).round().clamp(0.0, 15.0) as u64;
        code |= normalized << (i * 4);
    }
    let code = code % state_space;

    match previous_id {
        None => Some(code),
        Some(prev) => {
            if code == prev {
                return None;
            }
            let dist = cyclic_forward_distance(prev, code, state_space);
            if dist == 0 || dist > max_forward_jump_codes {
                None
            } else {
                Some(code)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quad_code_decodes_forward_sequence() {
        let mut prev = None;
        let state_space = 65_536;

        for expected in 0..20u64 {
            let code = (expected % state_space) as u16;
            let lumas = [
                16.0 + ((code & 0xF) as f32) * 14.0,
                16.0 + (((code >> 4) & 0xF) as f32) * 14.0,
                16.0 + (((code >> 8) & 0xF) as f32) * 14.0,
                16.0 + (((code >> 12) & 0xF) as f32) * 14.0,
            ];
            let id = decode_quad_code(lumas, prev, state_space, 4);
            assert_eq!(id, Some(expected % state_space));
            prev = id;
        }
    }

    #[test]
    fn cyclic_distance_wraps() {
        assert_eq!(cyclic_forward_distance(65530, 2, 65536), 8);
    }
}
