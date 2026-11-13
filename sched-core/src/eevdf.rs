//! EEVDF arithmetic in kernel virtual time.
//!
//! Virtual time is measured in nanoseconds but advances at a rate inversely
//! proportional to the entity's weight. Weight 1024 is the neutral point:
//! running for one millisecond advances virtual time by one millisecond.
//!
//! A requested slice is a deadline request, not a promise that the entity will
//! run for that long without interruption.

/// Neutral weight. Larger weights receive proportionally more CPU time and
/// proportionally smaller virtual-time increments.
pub const WEIGHT_SCALE: u64 = 1024;

pub const MIN_WEIGHT: u32 = 16;
pub const MAX_WEIGHT: u32 = 65_535;

/// Shortest slice the kernel will request.
pub const MIN_SLICE_NS: u64 = 500_000;
/// Longest slice the kernel will request.
pub const MAX_SLICE_NS: u64 = 50_000_000;

/// Symmetric lag clamp around the run queue's floor virtual time.
pub const LAG_LIMIT_NS: u64 = 6_000_000;

/// Convert a wall-clock duration into this weight's virtual-time charge.
///
/// The result is at least one nanosecond so a very large weight cannot make an
/// entity's virtual time stand still.
pub fn scaled_slice(nanoseconds: u64, weight: u32) -> u64 {
    let weight = if weight < MIN_WEIGHT {
        MIN_WEIGHT
    } else if weight > MAX_WEIGHT {
        MAX_WEIGHT
    } else {
        weight
    } as u64;

    let scaled = nanoseconds.saturating_mul(WEIGHT_SCALE) / weight;
    if scaled == 0 {
        1
    } else {
        scaled
    }
}

/// Charge an entity for wall-clock time it actually ran.
pub fn advance(virtual_time: u64, ran_ns: u64, weight: u32) -> u64 {
    virtual_time.saturating_add(scaled_slice(ran_ns, weight))
}

/// Compute the virtual deadline for a requested slice.
pub fn deadline(virtual_time: u64, slice_ns: u64, weight: u32) -> u64 {
    virtual_time.saturating_add(scaled_slice(slice_ns, weight))
}

/// Clamp an entity's virtual time to a symmetric window around the run queue's
/// floor. A sleeper cannot bank unlimited lag, and a repeatedly preempted
/// entity cannot be pushed arbitrarily far ahead.
pub fn clamp_lag(virtual_time: u64, floor: u64) -> u64 {
    let low = floor.saturating_sub(LAG_LIMIT_NS);
    let high = floor.saturating_add(LAG_LIMIT_NS);
    if virtual_time < low {
        low
    } else if virtual_time > high {
        high
    } else {
        virtual_time
    }
}

/// Linux-style EEVDF eligibility.
///
/// An entity is eligible when its virtual time is at or below the run queue's
/// load-weighted average virtual time, i.e. it is owed service rather than
/// ahead of the queue. `weight_sum` and `weighted_vtime_sum` must cover the
/// same set of on-queue entities (runnable plus the running one).
///
/// With `W = sum(weight)` and `S = sum(vtime * weight)` this is
/// `vtime * W <= S`, which is the `vruntime - min_vruntime <= avg_vruntime`
/// test from Linux with `min_vruntime` cancelled on both sides. The kernel
/// computes it with ordinary `u64` values and passes them here; the comparison
/// widens to `u128` so a large table cannot overflow.
pub fn eligible(vtime: u64, weight_sum: u64, weighted_vtime_sum: u64) -> bool {
    if weight_sum == 0 {
        return true;
    }
    (vtime as u128) * (weight_sum as u128) <= weighted_vtime_sum as u128
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_weight_advances_one_to_one() {
        assert_eq!(scaled_slice(1_000_000, 1024), 1_000_000);
        assert_eq!(advance(5, 2_000_000, 1024), 2_000_005);
        assert_eq!(deadline(10, 3_000_000, 1024), 3_000_010);
    }

    #[test]
    fn higher_weight_advances_slower() {
        assert_eq!(scaled_slice(1_000_000, 2048), 500_000);
        assert_eq!(scaled_slice(1_000_000, 512), 2_000_000);
        // A weight below the accepted minimum is normalised.
        assert_eq!(scaled_slice(1_000_000, 0), scaled_slice(1_000_000, MIN_WEIGHT));
    }

    #[test]
    fn short_slices_get_earlier_deadlines() {
        let short = deadline(100, 1_000_000, 1024);
        let long = deadline(100, 4_000_000, 1024);
        assert!(short < long);
    }

    #[test]
    fn lag_clamp_is_symmetric_and_saturating() {
        assert_eq!(clamp_lag(1_000, 10_000_000), 4_000_000);
        assert_eq!(clamp_lag(20_000_000, 10_000_000), 16_000_000);
        assert_eq!(clamp_lag(10_000_000, 10_000_000), 10_000_000);
        assert_eq!(clamp_lag(0, 0), 0);
        assert_eq!(clamp_lag(u64::MAX, 0), LAG_LIMIT_NS);
    }

    #[test]
    fn eligibility_uses_weighted_average_virtual_time() {
        // Two entities at the same virtual time are both eligible.
        assert!(eligible(100, 2048, 100 * 2048));
        // An entity behind the average is eligible.
        assert!(eligible(50, 2048, 100 * 2048));
        // An entity ahead of the average is not.
        assert!(!eligible(150, 2048, 100 * 2048));
        // An empty queue has no one to deny.
        assert!(eligible(150, 0, 0));
    }

    #[test]
    fn eligibility_does_not_overflow_for_large_tables() {
        // 32 entities at maximum weight, one hour of virtual time.
        let vtime = 3_600_000_000_000u64;
        let weight_sum = 65_535 * 32;
        let weighted_sum = vtime * weight_sum;
        assert!(eligible(vtime, weight_sum, weighted_sum));
        assert!(eligible(vtime - 1, weight_sum, weighted_sum));
        assert!(!eligible(vtime + 1, weight_sum, weighted_sum));
    }
}
