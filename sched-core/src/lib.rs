//! Scheduling primitives shared by the SketchOS kernel and host tests.
//!
//! The crate is deliberately independent of the kernel's entity table. It
//! provides:
//!
//! * [`avl::RunTree`] - a fixed-capacity indexed AVL tree keyed by
//!   `(virtual deadline, entity id)`.
//! * [`eevdf`] - virtual-time, deadline, eligibility and lag-clamp arithmetic.
//! * [`latency::LatencyStats`] - count/sum/max plus a log2 histogram used for
//!   wake-latency and run-delay reports.
//! * [`wait::WaitQueue`] - a small FIFO of entity ids for blocked waiters.
//!
//! Nothing here allocates, and every container has a compile-time capacity.

#![cfg_attr(not(test), no_std)]

pub mod avl;
pub mod eevdf;
pub mod latency;
pub mod wait;

pub use avl::{Key, RunTree, TreeError};
pub use eevdf::{
    advance, clamp_lag, deadline, eligible, scaled_slice, LAG_LIMIT_NS, MAX_SLICE_NS,
    MAX_WEIGHT, MIN_SLICE_NS, MIN_WEIGHT, WEIGHT_SCALE,
};
pub use latency::LatencyStats;
pub use wait::WaitQueue;
