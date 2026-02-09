// Format hash rate into human-readable form with appropriate unit
pub fn format_hash_rate(hps: f64) -> (f64, &'static str) {
    const UNITS: [&str; 5] = ["H/s", "kH/s", "MH/s", "GH/s", "TH/s"];

    let mut rate = hps;
    let mut unit = 0;

    while rate >= 1000.0 && unit < UNITS.len() - 1 {
        rate /= 1000.0;
        unit += 1;
    }

    (rate, UNITS[unit])
}

// Convert a 32-byte difficulty target to a normalized difficulty value
pub fn normalize_difficulty(target: &[u8; 32]) -> f64 {
    // Find first non-zero byte
    let mut i = 0;
    while i < 32 && target[i] == 0 {
        i += 1;
    }

    if i == 32 {
        return f64::INFINITY;
    }

    // Read top 8 bytes for mantissa
    let mut buf = [0u8; 8];
    let len = (32 - i).min(8);
    buf[..len].copy_from_slice(&target[i..i + len]);

    let mantissa = u64::from_be_bytes(buf) as f64;

    // Calculate exponent in bits
    let exp = ((32 - i) * 8) as i32;

    // Max target is all 0xff bytes
    let max_mantissa = u64::MAX as f64;
    let max_exp = 256i32;

    let target_f = mantissa * 2f64.powi(exp - 64);
    let max_f = max_mantissa * 2f64.powi(max_exp - 64);

    max_f / target_f
}
