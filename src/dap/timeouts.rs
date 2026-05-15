//! Configurable per-request DAP timeouts.
//!
//! The defaults are tuned for fast adapters with small debug binaries. Large
//! Rust binaries (hundreds of MB of debug info) can blow past 8s on the first
//! evaluate while LLDB walks DWARF — see the holon incident write-up. Operators
//! can widen these via env vars without rebuilding.
//!
//! All values are clamped to `[min, MAX_MS]` so an accidentally-large env value
//! can't make a stuck request invisible.

use std::time::Duration;

/// Hard upper bound on any single DAP request timeout. A request that takes
/// longer than 10 minutes is almost certainly stuck; we want the timeout to
/// fire so the recovery path runs.
const MAX_MS: u64 = 10 * 60 * 1000;

fn resolve_ms(raw: Option<&str>, default_ms: u64, min_ms: u64) -> Duration {
    let ms = raw
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default_ms)
        .clamp(min_ms, MAX_MS);
    Duration::from_millis(ms)
}

fn read_env_ms(var: &str, default_ms: u64, min_ms: u64) -> Duration {
    resolve_ms(std::env::var(var).ok().as_deref(), default_ms, min_ms)
}

/// Timeout for `evaluate` requests. Default 8s.
/// Override with `DAP_EVAL_TIMEOUT_MS`.
pub fn evaluate() -> Duration {
    read_env_ms("DAP_EVAL_TIMEOUT_MS", 8_000, 100)
}

/// Timeout for `variables` requests. Default 4s for paginated, 10s for full.
/// Override with `DAP_VARIABLES_TIMEOUT_MS` / `DAP_VARIABLES_FULL_TIMEOUT_MS`.
pub fn variables_limited() -> Duration {
    read_env_ms("DAP_VARIABLES_TIMEOUT_MS", 4_000, 100)
}

pub fn variables_full() -> Duration {
    read_env_ms("DAP_VARIABLES_FULL_TIMEOUT_MS", 10_000, 100)
}

/// Timeout for `stackTrace` requests. Default 10s.
/// Override with `DAP_STACK_TRACE_TIMEOUT_MS`.
pub fn stack_trace() -> Duration {
    read_env_ms("DAP_STACK_TRACE_TIMEOUT_MS", 10_000, 100)
}

/// Timeout for `scopes` requests. Default 10s.
/// Override with `DAP_SCOPES_TIMEOUT_MS`.
pub fn scopes() -> Duration {
    read_env_ms("DAP_SCOPES_TIMEOUT_MS", 10_000, 100)
}

#[cfg(test)]
mod tests {
    // Test the pure parser, not the env-reading wrapper — tests run in parallel
    // and global env state can't be safely mutated from concurrent tests.

    use super::*;

    #[test]
    fn default_used_when_unset() {
        assert_eq!(resolve_ms(None, 8_000, 100), Duration::from_secs(8));
    }

    #[test]
    fn explicit_value_overrides_default() {
        assert_eq!(resolve_ms(Some("60000"), 8_000, 100), Duration::from_secs(60));
    }

    #[test]
    fn value_above_max_is_clamped() {
        assert_eq!(
            resolve_ms(Some("9999999999"), 8_000, 100),
            Duration::from_millis(MAX_MS)
        );
    }

    #[test]
    fn value_below_min_is_clamped() {
        assert_eq!(resolve_ms(Some("1"), 8_000, 100), Duration::from_millis(100));
    }

    #[test]
    fn garbage_falls_back_to_default() {
        assert_eq!(
            resolve_ms(Some("not-a-number"), 8_000, 100),
            Duration::from_secs(8)
        );
    }
}
