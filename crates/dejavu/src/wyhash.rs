//! `Bun.hash(bytes)`: Zig's `std.hash.Wyhash` with seed 0. The transcript
//! index stores it (as a decimal string) for the first 4 KiB of each JSONL file,
//! so the Rust and Bun binaries must agree on it to share one index.

const SECRET: [u64; 4] = [
    0xa076_1d64_78bd_642f,
    0xe703_7ed1_a0b4_28db,
    0x8ebc_6af0_9c88_c6e3,
    0x5899_65cc_7537_4cc3,
];

fn mum(a: u64, b: u64) -> (u64, u64) {
    let x = u128::from(a).wrapping_mul(u128::from(b));
    (x as u64, (x >> 64) as u64)
}

fn mix(a: u64, b: u64) -> u64 {
    let (a, b) = mum(a, b);
    a ^ b
}

fn read8(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn read4(bytes: &[u8], at: usize) -> u64 {
    u64::from(u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()))
}

/// Wyhash of `input` with `seed`.
pub fn wyhash(seed: u64, input: &[u8]) -> u64 {
    let mut state = [seed ^ mix(seed ^ SECRET[0], SECRET[1]); 3];
    let (a, b);
    let len = input.len();
    if len <= 16 {
        if len >= 4 {
            let end = len - 4;
            let quarter = (len >> 3) << 2;
            a = (read4(input, 0) << 32) | read4(input, quarter);
            b = (read4(input, end) << 32) | read4(input, end - quarter);
        } else if len > 0 {
            a = (u64::from(input[0]) << 16)
                | (u64::from(input[len >> 1]) << 8)
                | u64::from(input[len - 1]);
            b = 0;
        } else {
            a = 0;
            b = 0;
        }
    } else {
        let mut i = 0;
        if len >= 48 {
            while i + 48 < len {
                for (k, slot) in state.iter_mut().enumerate() {
                    let x = read8(input, i + 16 * k);
                    let y = read8(input, i + 16 * k + 8);
                    *slot = mix(x ^ SECRET[k + 1], y ^ *slot);
                }
                i += 48;
            }
            state[0] ^= state[1] ^ state[2];
        }
        let rest = &input[i..];
        let mut j = 0;
        while j + 16 < rest.len() {
            state[0] = mix(read8(rest, j) ^ SECRET[1], read8(rest, j + 8) ^ state[0]);
            j += 16;
        }
        a = read8(input, len - 16);
        b = read8(input, len - 8);
    }
    let (a, b) = mum(a ^ SECRET[1], b ^ state[0]);
    mix(a ^ SECRET[0] ^ len as u64, b ^ SECRET[1])
}

/// `String(Bun.hash(bytes))`.
pub fn bun_hash(input: &[u8]) -> String {
    wyhash(0, input).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_bun_hash() {
        assert_eq!(bun_hash(b""), "290873116282709081");
        assert_eq!(bun_hash(b"hello"), "1019145960556548909");
        assert_eq!(bun_hash("a".repeat(100).as_bytes()), "7077612499900502782");
    }
}
