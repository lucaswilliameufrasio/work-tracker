//! Development-only KDF calibration. This is not password-hashing product code.
use std::{hint::black_box, thread, time::Instant};

use argon2::{Algorithm, Argon2, Params, Version};
use serde_json::{Value, json};

const PASSWORD: &[u8] = b"synthetic-calibration-password";
const SALT: &[u8] = b"synthetic-calibration-salt";
const OUTPUT_LEN: usize = 32;
const SAMPLES: usize = 50;

fn hash_once(memory_kib: u32, iterations: u32) -> [u8; OUTPUT_LEN] {
    let params = Params::new(memory_kib, iterations, 1, Some(OUTPUT_LEN)).unwrap();
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut output = [0; OUTPUT_LEN];
    argon2
        .hash_password_into(PASSWORD, SALT, &mut output)
        .unwrap();
    output
}

fn percentile(sorted: &[f64], percent: usize) -> f64 {
    sorted[(sorted.len() * percent).div_ceil(100).saturating_sub(1)]
}

fn time_samples(memory_kib: u32, iterations: u32) -> Value {
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let start = Instant::now();
        black_box(hash_once(memory_kib, iterations));
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(f64::total_cmp);
    json!({
        "memory_kib": memory_kib,
        "iterations": iterations,
        "parallelism": 1,
        "samples": SAMPLES,
        "median_ms": samples[samples.len() / 2],
        "p95_ms": percentile(&samples, 95),
    })
}

/// Benchmark sequential hashes and burst concurrency under the caller's cgroup.
pub fn measure() -> Value {
    let sequential = [
        time_samples(19 * 1024, 2),
        time_samples(19 * 1024, 3),
        time_samples(32 * 1024, 2),
        time_samples(64 * 1024, 3),
    ];
    let concurrent = [1, 2, 4, 8]
        .into_iter()
        .map(|concurrency| {
            let start = Instant::now();
            let threads: Vec<_> = (0..concurrency)
                .map(|_| thread::spawn(|| black_box(hash_once(64 * 1024, 3))))
                .collect();
            for thread in threads {
                thread.join().expect("Argon2 benchmark worker panicked");
            }
            json!({
                "concurrent_hashes": concurrency,
                "batch_wall_ms": start.elapsed().as_secs_f64() * 1000.0,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "workload": "synthetic Argon2id v19 hash_password_into; NOT product password flow or capacity promise",
        "sequential": sequential,
        "concurrency_burst": concurrent,
        "process_status_after_kdf": std::fs::read_to_string("/proc/self/status").ok().map(|text| {
            text.lines()
                .filter(|line| line.starts_with("VmRSS:") || line.starts_with("VmHWM:"))
                .collect::<Vec<_>>()
                .join("\n")
        }),
        "cgroup_cpu_max": std::fs::read_to_string("/sys/fs/cgroup/cpu.max").ok(),
        "cgroup_memory_max": std::fs::read_to_string("/sys/fs/cgroup/memory.max").ok(),
        // memory.peak cannot be reset by this unprivileged container. The value
        // is the whole run's high-water after the KDF burst, not an isolated KDF delta.
        "cgroup_memory_peak_after_kdf": std::fs::read_to_string("/sys/fs/cgroup/memory.peak").ok(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_hash_is_deterministic_and_salt_sensitive() {
        assert_eq!(hash_once(19 * 1024, 2), hash_once(19 * 1024, 2));
        let params = Params::new(19 * 1024, 2, 1, Some(OUTPUT_LEN)).unwrap();
        let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let mut other_salt = [0; OUTPUT_LEN];
        argon2
            .hash_password_into(PASSWORD, b"different-salt", &mut other_salt)
            .unwrap();
        assert_ne!(hash_once(19 * 1024, 2), other_salt);
    }
}
