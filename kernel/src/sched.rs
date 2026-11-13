//! Per-core EEVDF scheduler with cluster-aware work stealing.
//!
//! # Model
//!
//! An *entity* is the schedulable unit. Kernel demo workers and user threads
//! are both entities. Each entity carries a weight, a requested slice, a
//! virtual time and a virtual deadline. Every core owns a fixed-capacity AVL
//! tree keyed by `(virtual deadline, entity id)`; there is no global runnable
//! queue.
//!
//! Virtual time is nanoseconds anchored to the same clock the timer uses, so a
//! tree deadline is also a useful wall-clock hint for the next event. The
//! requested slice is a *deadline request*: an entity with an earlier deadline
//! may preempt it before the slice is exhausted.
//!
//! # Locking
//!
//! One IRQ-safe lock serialises all scheduler state. The per-core run trees,
//! timer list and entity table live inside it. This milestone deliberately uses
//! one lock instead of separate run-queue locks: every operation that touches
//! two cores (stealing, balancing) is then a single critical section, and the
//! invariant checker can inspect a consistent snapshot. It is a serialisation
//! point, not a global run queue, and the documentation says so.
//!
//! The lock order is `process table -> scheduler`. The scheduler never takes
//! the process-table lock: user entities carry their TTBR0 value, so an address
//! space switch is possible from the timer path.
//!
//! # Context switching
//!
//! The exception vectors build the 816-byte [`TrapFrame`] on the interrupted
//! thread's stack. A switch stores that frame pointer in the entity and returns
//! another entity's saved frame pointer, which the assembly return path
//! installs with `eret`.
//!
//! # Idle
//!
//! A core with no runnable entity and no timer wait masks `CNTV` and executes
//! `WFI`. There is no periodic tick. Another core wakes it with `SGI_RESCHED`
//! after pushing or stealing work.

use core::cell::UnsafeCell;
use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicU16, AtomicU64, Ordering};

use sketchos_sched_core::{
    advance, clamp_lag, deadline, eligible, Key, LatencyStats, RunTree,
};

use crate::arch::aarch64::{enter_task, gic, mmu, timer, TrapFrame};
use crate::percpu::{self, MAX_SMP_CORES};
use crate::sync::IrqSpinLock;
use crate::topology;
use crate::{println, smp};

/// Largest entity table. Eleven kernel workers plus the user-phase threads
/// fit comfortably; the cap keeps every structure static.
pub const MAX_ENTITIES: usize = 32;

/// CPU-bound floating-point workers started by the kernel demo.
pub const KERNEL_WORKERS: usize = 11;

/// Purposefully long demo so every worker and every online core gets to run.
pub const DEMO_DURATION_NS: u64 = 1_000_000_000;

/// Neutral weight for the kernel demo.
pub const DEMO_WEIGHT: u32 = 1024;
/// Requested slice for a kernel demo worker.
pub const DEMO_SLICE_NS: u64 = 4_000_000;

/// Distance between two work-stealing attempts from one core.
const BALANCE_COOLDOWN_NS: u64 = 500_000;

const STACK_SIZE: usize = 16 * 1024;
const GUARD_SIZE: usize = 4096;

const NO_ID: u32 = u32::MAX;

const KIND_KERNEL: u8 = 0;
const KIND_USER: u8 = 1;

const ST_UNUSED: u8 = 0;
const ST_RUNNABLE: u8 = 1;
const ST_RUNNING: u8 = 2;
const ST_BLOCKED: u8 = 3;
const ST_SLEEPING: u8 = 4;
const ST_EXITED: u8 = 5;

const SPSR_EL1H_IRQ_ON: u64 = 0x5;
const SPSR_EL1H_IRQ_OFF: u64 = 0x3c5;

#[repr(C, align(4096))]
struct Stack {
    /// Never accessed; [`install_guard_pages`] removes this page from the map.
    _guard: [u8; GUARD_SIZE],
    data: [u8; STACK_SIZE],
}

struct StackSet<const N: usize>(UnsafeCell<[Stack; N]>);

// SAFETY: a kernel worker stack is used by exactly one entity, and an idle
// stack by exactly one core. Both are written before their first `eret`.
unsafe impl<const N: usize> Sync for StackSet<N> {}

static KERNEL_STACKS: StackSet<MAX_ENTITIES> = StackSet(UnsafeCell::new([const {
    Stack {
        _guard: [0; GUARD_SIZE],
        data: [0; STACK_SIZE],
    }
}; MAX_ENTITIES]));

static IDLE_STACKS: StackSet<MAX_SMP_CORES> = StackSet(UnsafeCell::new([const {
    Stack {
        _guard: [0; GUARD_SIZE],
        data: [0; STACK_SIZE],
    }
}; MAX_SMP_CORES]));

fn kernel_stack_base(id: usize) -> u64 {
    debug_assert!(id < MAX_ENTITIES);
    // SAFETY: the caller owns `id` and it is in range.
    let stacks = unsafe { &mut *KERNEL_STACKS.0.get() };
    stacks[id]._guard.as_mut_ptr() as u64
}

fn kernel_stack_top(id: usize) -> u64 {
    kernel_stack_base(id) + size_of::<Stack>() as u64
}

fn idle_stack_base(core: usize) -> u64 {
    debug_assert!(core < MAX_SMP_CORES);
    // SAFETY: the caller owns `core` and it is in range.
    let stacks = unsafe { &mut *IDLE_STACKS.0.get() };
    stacks[core]._guard.as_mut_ptr() as u64
}

fn idle_stack_top(core: usize) -> u64 {
    idle_stack_base(core) + size_of::<Stack>() as u64
}

/// Per-worker iteration counters, for the final report only.
static WORK: [AtomicU64; KERNEL_WORKERS] = [const { AtomicU64::new(0) }; KERNEL_WORKERS];
/// Per-worker floating-point accumulator bits, proving the FP path ran.
static FP_BITS: [AtomicU64; KERNEL_WORKERS] = [const { AtomicU64::new(0) }; KERNEL_WORKERS];

/// Cores currently waiting in `WFI`, one bit per logical core.
static IDLE_MASK: AtomicU16 = AtomicU16::new(0);

#[derive(Clone, Copy)]
struct Entity {
    used: bool,
    kind: u8,
    state: u8,
    id: u32,
    weight: u32,
    slice_ns: u64,
    vtime: u64,
    deadline: u64,
    remaining_ns: u64,
    core: u8,
    affinity: u16,
    frame: u64,
    stack_top: u64,
    ttbr0: u64,
    pid: u32,
    tid: u32,
    wake_at_ns: u64,
    runnable_since_ns: u64,
    last_start_ns: u64,
    run_ns: u64,
    wake_latency: LatencyStats,
    run_delay: LatencyStats,
}

impl Entity {
    const EMPTY: Self = Self {
        used: false,
        kind: KIND_KERNEL,
        state: ST_UNUSED,
        id: NO_ID,
        weight: DEMO_WEIGHT,
        slice_ns: DEMO_SLICE_NS,
        vtime: 0,
        deadline: 0,
        remaining_ns: 0,
        core: 0,
        affinity: 0,
        frame: 0,
        stack_top: 0,
        ttbr0: 0,
        pid: 0,
        tid: 0,
        wake_at_ns: 0,
        runnable_since_ns: 0,
        last_start_ns: 0,
        run_ns: 0,
        wake_latency: LatencyStats::new(),
        run_delay: LatencyStats::new(),
    };
}

#[derive(Clone, Copy)]
struct TimerWait {
    id: u32,
    at_ns: u64,
}

impl TimerWait {
    const EMPTY: Self = Self { id: NO_ID, at_ns: 0 };
}

struct CoreSched {
    tree: RunTree<MAX_ENTITIES>,
    current: u32,
    ready: bool,
    /// This core's demo window has closed. The shared `demo_end_ns` is no
    /// longer a wake event for it.
    finished: bool,
    ticks: u64,
    switches: u64,
    steals: u64,
    next_event_ns: u64,
    balance_sent_ns: u64,
}

impl CoreSched {
    const EMPTY: Self = Self {
        tree: RunTree::new(),
        current: NO_ID,
        ready: false,
        finished: false,
        ticks: 0,
        switches: 0,
        steals: 0,
        next_event_ns: 0,
        balance_sent_ns: 0,
    };
}

struct State {
    entities: [Entity; MAX_ENTITIES],
    cores: [CoreSched; MAX_SMP_CORES],
    timers: [TimerWait; MAX_ENTITIES],
    timer_count: usize,
    next_id: u32,
    demo_start_ns: u64,
    demo_end_ns: u64,
    demo_active: bool,
    user_active: bool,
    current_user: u32,
    ready: bool,
    armed_events: u64,
    disarms: u64,
    steals: u64,
    same_cluster_steals: u64,
    cross_cluster_steals: u64,
    migrations: u64,
    balance_sgis: u64,
    checks: u64,
    violations: u64,
}

impl State {
    const EMPTY: Self = Self {
        entities: [const { Entity::EMPTY }; MAX_ENTITIES],
        cores: [const { CoreSched::EMPTY }; MAX_SMP_CORES],
        timers: [const { TimerWait::EMPTY }; MAX_ENTITIES],
        timer_count: 0,
        next_id: 0,
        demo_start_ns: 0,
        demo_end_ns: 0,
        demo_active: false,
        user_active: false,
        current_user: NO_ID,
        ready: false,
        armed_events: 0,
        disarms: 0,
        steals: 0,
        same_cluster_steals: 0,
        cross_cluster_steals: 0,
        migrations: 0,
        balance_sgis: 0,
        checks: 0,
        violations: 0,
    };

    fn core_count(&self) -> usize {
        topology::core_count().clamp(1, MAX_SMP_CORES)
    }

    fn floor(&self, core: usize) -> u64 {
        let mut floor = u64::MAX;
        for entity in &self.entities {
            if !entity.used || entity.core as usize != core {
                continue;
            }
            if entity.state == ST_RUNNABLE || entity.state == ST_RUNNING {
                floor = floor.min(entity.vtime);
            }
        }
        if floor == u64::MAX {
            timer::now_ns()
        } else {
            floor
        }
    }

    /// Load-weighted virtual time of the entities on this core's run queue,
    /// including the running entity. Returns `(sum(weight), sum(vtime*weight))`.
    fn weighted_average(&self, core: usize) -> (u64, u64) {
        let mut weight_sum = 0u64;
        let mut weighted_sum = 0u64;
        for entity in &self.entities {
            if !entity.used || entity.core as usize != core {
                continue;
            }
            if entity.state != ST_RUNNABLE && entity.state != ST_RUNNING {
                continue;
            }
            let weight = entity.weight as u64;
            weight_sum = weight_sum.saturating_add(weight);
            weighted_sum = weighted_sum.saturating_add(entity.vtime.saturating_mul(weight));
        }
        (weight_sum, weighted_sum)
    }

    fn save_frame(&mut self, core: usize, frame: *mut TrapFrame) {
        let current = self.cores[core].current;
        if current != NO_ID {
            self.entities[current as usize].frame = frame as u64;
        }
    }

    fn charge(&mut self, id: u32, now: u64) {
        let entity = &mut self.entities[id as usize];
        let ran = now.saturating_sub(entity.last_start_ns);
        entity.vtime = advance(entity.vtime, ran, entity.weight);
        entity.run_ns = entity.run_ns.saturating_add(ran);
        if ran >= entity.remaining_ns {
            entity.remaining_ns = 0;
        } else {
            entity.remaining_ns -= ran;
        }
    }

    /// Yield forfeits the remainder of the requested slice, so a yield-spin
    /// loop cannot monopolise the core by repeatedly giving up a few
    /// nanoseconds.
    fn charge_yield(&mut self, id: u32, now: u64) {
        let entity = &mut self.entities[id as usize];
        let ran = now.saturating_sub(entity.last_start_ns);
        entity.run_ns = entity.run_ns.saturating_add(ran);
        entity.vtime = advance(entity.vtime, entity.slice_ns, entity.weight);
        entity.remaining_ns = 0;
    }

    fn requeue(&mut self, id: u32, now: u64) {
        let core = self.entities[id as usize].core as usize;
        let floor = self.floor(core);
        let entity = &mut self.entities[id as usize];
        entity.vtime = clamp_lag(entity.vtime, floor);
        if entity.remaining_ns == 0 {
            entity.deadline = deadline(entity.vtime, entity.slice_ns, entity.weight);
            entity.remaining_ns = entity.slice_ns;
        }
        entity.state = ST_RUNNABLE;
        entity.runnable_since_ns = now;
        let key = Key::new(entity.deadline, id);
        let _ = self.cores[core].tree.insert(key);
    }

    fn dispatch(&mut self, id: u32, core: usize, now: u64) -> *mut TrapFrame {
        let floor = self.floor(core);
        let (frame, kind) = {
            let entity = &mut self.entities[id as usize];
            entity.vtime = clamp_lag(entity.vtime, floor);
            if entity.remaining_ns == 0 {
                entity.deadline = deadline(entity.vtime, entity.slice_ns, entity.weight);
                entity.remaining_ns = entity.slice_ns;
            }
            entity.state = ST_RUNNING;
            entity.core = core as u8;
            entity.last_start_ns = now;
            let delay = now.saturating_sub(entity.runnable_since_ns);
            entity.run_delay.record(delay);
            if entity.kind == KIND_USER && entity.wake_at_ns != 0 {
                let latency = now.saturating_sub(entity.wake_at_ns);
                entity.wake_latency.record(latency);
                entity.wake_at_ns = 0;
            }
            (entity.frame as *mut TrapFrame, entity.kind)
        };

        if self.entities[id as usize].ttbr0 != 0 {
            mmu::install_ttbr0(self.entities[id as usize].ttbr0);
        }
        self.cores[core].current = id;
        self.cores[core].switches = self.cores[core].switches.saturating_add(1);
        if let Some(record) = percpu::record(core) {
            record.sched_switches.fetch_add(1, Ordering::Relaxed);
            record.sched_current.store(id, Ordering::Relaxed);
        }
        if kind == KIND_USER {
            self.current_user = id;
        }
        IDLE_MASK.fetch_and(!(1u16 << core), Ordering::Release);
        frame
    }

    /// Stop the current entity, charging it if it was still running, then pick
    /// and dispatch the next entity on `core`.
    fn select_and_dispatch(&mut self, core: usize, now: u64) -> Option<*mut TrapFrame> {
        let current = self.cores[core].current;
        if current != NO_ID {
            if self.entities[current as usize].state == ST_RUNNING {
                self.charge(current, now);
                self.requeue(current, now);
            }
            self.cores[core].current = NO_ID;
            if self.current_user == current {
                self.current_user = NO_ID;
            }
        }

        let id = self.select(core, now)?;
        Some(self.dispatch(id, core, now))
    }

    fn select(&mut self, core: usize, now: u64) -> Option<u32> {
        let (weight_sum, weighted_sum) = self.weighted_average(core);
        let mut chosen: Option<Key> = None;
        let mut fallback: Option<Key> = None;
        {
            let entities = &self.entities;
            let tree = &self.cores[core].tree;
            tree.for_each(&mut |key: Key| {
                let entity = &entities[key.id as usize];
                // Keys for retired entities are a bug, but never dispatch one.
                if !entity.used || entity.state != ST_RUNNABLE {
                    return;
                }
                if fallback.is_none() {
                    fallback = Some(key);
                }
                if chosen.is_none() && eligible(entity.vtime, weight_sum, weighted_sum) {
                    chosen = Some(key);
                }
            });
        }
        // Nothing eligible? Take the earliest deadline anyway: a core must not
        // idle while a runnable entity exists. Remove the key that was actually
        // observed, not one recomputed from the entity, so a stale key cannot
        // survive a dispatch.
        if let Some(key) = chosen.or(fallback) {
            let _ = self.cores[core].tree.remove(key);
            return Some(key.id);
        }

        if let Some(id) = self.steal(core, now) {
            // `steal` moved the key into this core's tree. The entity is about
            // to run, so its key must leave the tree now; leaving it would make
            // the entity both running and queued.
            let key = Key::new(self.entities[id as usize].deadline, id);
            let _ = self.cores[core].tree.remove(key);
            return Some(id);
        }

        self.request_balance(core, now);
        None
    }

    /// Steal the latest-deadline entity, preferring victims in the same
    /// cluster. The latest deadline is the cheapest entity to give away: it is
    /// the one that would run last on the victim anyway.
    fn steal(&mut self, thief: usize, now: u64) -> Option<u32> {
        let thief_cluster = topology::cluster_of(thief);
        let _ = thief_cluster;
        let core_count = self.core_count();
        for pass in 0..2 {
            for victim in 0..core_count {
                if victim == thief {
                    continue;
                }
                let same = topology::same_cluster(victim, thief);
                if pass == 0 && !same {
                    continue;
                }
                if pass == 1 && same {
                    continue;
                }
                let Some(key) = self.cores[victim].tree.last() else {
                    continue;
                };
                let id = key.id;
                if id as usize >= MAX_ENTITIES {
                    continue;
                }
                let entity = &self.entities[id as usize];
                if !entity.used || entity.state != ST_RUNNABLE {
                    continue;
                }
                if entity.affinity & (1u16 << thief) == 0 {
                    continue;
                }
                if self.cores[victim].tree.remove(key).is_err() {
                    continue;
                }
                if self.cores[thief].tree.insert(key).is_err() {
                    let _ = self.cores[victim].tree.insert(key);
                    continue;
                }
                {
                    let entity = &mut self.entities[id as usize];
                    entity.core = thief as u8;
                    entity.state = ST_RUNNABLE;
                    entity.runnable_since_ns = now;
                }
                self.cores[thief].steals = self.cores[thief].steals.saturating_add(1);
                self.steals = self.steals.saturating_add(1);
                self.migrations = self.migrations.saturating_add(1);
                if same {
                    self.same_cluster_steals = self.same_cluster_steals.saturating_add(1);
                } else {
                    self.cross_cluster_steals = self.cross_cluster_steals.saturating_add(1);
                }
                return Some(id);
            }
        }
        None
    }

    /// Ask the busiest core to push a runnable entity to an idle core. The
    /// request is rate-limited per core and delivered with `SGI_BALANCE`.
    fn request_balance(&mut self, core: usize, now: u64) {
        let elapsed = now.saturating_sub(self.cores[core].balance_sent_ns);
        if elapsed < BALANCE_COOLDOWN_NS {
            return;
        }
        let mut busiest = None;
        let mut best_len = 1usize;
        let core_count = self.core_count();
        for victim in 0..core_count {
            if victim == core {
                continue;
            }
            let len = self.cores[victim].tree.len();
            if len > best_len {
                best_len = len;
                busiest = Some(victim);
            }
        }
        let Some(victim) = busiest else {
            return;
        };
        self.cores[core].balance_sent_ns = now;
        if let Some(record) = percpu::record(victim) {
            let mpidr = record.mpidr.load(Ordering::Relaxed);
            let _ = gic::send_sgi(mpidr, gic::SGI_BALANCE);
            self.balance_sgis = self.balance_sgis.saturating_add(1);
        }
    }

    fn should_preempt(&self, core: usize, now: u64) -> bool {
        let current = self.cores[core].current;
        if current == NO_ID {
            return false;
        }
        let entity = &self.entities[current as usize];
        if entity.state != ST_RUNNING {
            return false;
        }
        let elapsed = now.saturating_sub(entity.last_start_ns);
        if elapsed >= entity.remaining_ns {
            return true;
        }
        let (weight_sum, weighted_sum) = self.weighted_average(core);
        if let Some(head) = self.cores[core].tree.first() {
            let head_entity = &self.entities[head.id as usize];
            if eligible(head_entity.vtime, weight_sum, weighted_sum)
                && head.deadline < entity.deadline
            {
                return true;
            }
        }
        false
    }

    fn add_timer(&mut self, id: u32, at_ns: u64) -> bool {
        if self.timer_count >= self.timers.len() {
            return false;
        }
        self.timers[self.timer_count] = TimerWait { id, at_ns };
        self.timer_count += 1;
        true
    }

    fn remove_timer(&mut self, id: u32) {
        let mut index = 0usize;
        while index < self.timer_count {
            if self.timers[index].id == id {
                self.timer_count -= 1;
                self.timers[index] = self.timers[self.timer_count];
                return;
            }
            index += 1;
        }
    }

    fn wake_timers(&mut self, now: u64) {
        let mut index = 0usize;
        while index < self.timer_count {
            if self.timers[index].at_ns <= now {
                let id = self.timers[index].id;
                self.timer_count -= 1;
                self.timers[index] = self.timers[self.timer_count];
                if id as usize >= MAX_ENTITIES {
                    continue;
                }
                let entity = &mut self.entities[id as usize];
                if entity.used && entity.state == ST_SLEEPING {
                    entity.state = ST_RUNNABLE;
                    entity.remaining_ns = 0;
                    self.requeue(id, now);
                }
            } else {
                index += 1;
            }
        }
    }

    fn next_event_ns(&self, core: usize, now: u64) -> u64 {
        let mut next = u64::MAX;
        if self.demo_active && !self.cores[core].finished {
            next = next.min(self.demo_end_ns);
        }
        // Only this core's timer waits are wake events for this core. The
        // timer list itself is global, and arming another core's timer would
        // make an otherwise tickless core wake for no work.
        for index in 0..self.timer_count {
            let id = self.timers[index].id as usize;
            if id < MAX_ENTITIES && self.entities[id].used && self.entities[id].core as usize == core {
                next = next.min(self.timers[index].at_ns);
            }
        }
        let current = self.cores[core].current;
        if current != NO_ID && self.entities[current as usize].state == ST_RUNNING {
            let remaining = self.entities[current as usize].remaining_ns.max(1);
            next = next.min(now.saturating_add(remaining));
        }
        if let Some(head) = self.cores[core].tree.first() {
            if head.deadline > now {
                next = next.min(head.deadline);
            }
        }
        next
    }

    fn arm_next_event(&mut self, core: usize, now: u64) {
        let next = self.next_event_ns(core, now);
        if next == u64::MAX {
            timer::disarm();
            self.disarms = self.disarms.saturating_add(1);
            self.cores[core].next_event_ns = 0;
        } else {
            let at = next.max(now.saturating_add(timer::MIN_EVENT_NS));
            timer::arm_at_ns(at);
            self.armed_events = self.armed_events.saturating_add(1);
            self.cores[core].next_event_ns = at;
        }
    }

    fn user_work_left(&self) -> bool {
        for entity in &self.entities {
            if entity.used
                && entity.kind == KIND_USER
                && (entity.state == ST_RUNNABLE
                    || entity.state == ST_RUNNING
                    || entity.state == ST_SLEEPING)
            {
                return true;
            }
        }
        false
    }

    /// Retire every kernel demo entity. Called once, when the boot core hands
    /// the CPU to the user phase. Without this, workers that were still queued
    /// when the demo window closed would keep winning deadlines and starve the
    /// user threads.
    fn end_demo(&mut self) {
        self.demo_active = false;
        for id in 0..MAX_ENTITIES {
            // Retire every kernel entity, including one that was already
            // marked exited but whose run-queue key survived: the user phase
            // must start with an empty kernel workload on every core.
            if self.entities[id].used && self.entities[id].kind == KIND_KERNEL {
                self.remove_entity(id as u32);
            }
        }
    }

    fn build_finish_frame(&self, core: usize) -> *mut TrapFrame {
        let address = idle_stack_top(core) - size_of::<TrapFrame>() as u64;
        let frame = address as *mut TrapFrame;
        // SAFETY: the idle stack belongs to this core and is not in use while a
        // worker or user thread is running. The frame is written under the
        // scheduler lock and IRQs are masked.
        unsafe {
            ptr::write_bytes(frame as *mut u8, 0, size_of::<TrapFrame>());
            (*frame).x[0] = core as u64;
            (*frame).elr = finish_task as *const () as usize as u64;
            (*frame).spsr = SPSR_EL1H_IRQ_OFF;
        }
        frame
    }

    fn build_idle_frame(&self, core: usize) -> *mut TrapFrame {
        let address = idle_stack_top(core) - size_of::<TrapFrame>() as u64;
        let frame = address as *mut TrapFrame;
        // SAFETY: see `build_finish_frame`.
        unsafe {
            ptr::write_bytes(frame as *mut u8, 0, size_of::<TrapFrame>());
            (*frame).x[0] = core as u64;
            (*frame).elr = idle_main as *const () as usize as u64;
            (*frame).spsr = SPSR_EL1H_IRQ_ON;
        }
        frame
    }

    /// Frame that ends the user phase after the last runnable user thread is
    /// gone. It reuses the current thread's kernel stack frame, which is
    /// abandoned, exactly like the old finish frame.
    fn finish_user_frame(&self, frame: *mut TrapFrame) -> *mut TrapFrame {
        // SAFETY: the current user thread is not resuming; its frame is at the
        // kernel stack pointer the exception vectors installed, and the caller
        // returns this frame to `restore_frame`.
        unsafe {
            ptr::write_bytes(frame as *mut u8, 0, size_of::<TrapFrame>());
            (*frame).elr = crate::process::user_phase_complete as *const () as usize as u64;
            (*frame).spsr = SPSR_EL1H_IRQ_OFF;
        }
        frame
    }

    fn finish_core_frame(&mut self, core: usize) -> *mut TrapFrame {
        self.cores[core].finished = true;
        let current = self.cores[core].current;
        if current != NO_ID {
            self.cores[core].current = NO_ID;
            // The demo worker is not resuming. Retire it completely so a key
            // left in a run queue cannot be scheduled during the user phase.
            self.remove_entity(current);
        }
        self.build_finish_frame(core)
    }

    /// Park a secondary core after its demo window. The empty run queue means
    /// there is no event to program, so the timer is disarmed and the core
    /// waits in `WFI`. This is the tickless-idle path: no periodic tick keeps
    /// a core that has nothing to run awake.
    fn park_idle(&mut self, core: usize) {
        self.cores[core].finished = true;
        for id in 0..MAX_ENTITIES {
            let entity = self.entities[id];
            if entity.used && entity.kind == KIND_KERNEL && entity.core as usize == core {
                self.remove_entity(id as u32);
            }
        }
        let now = timer::now_ns();
        self.arm_next_event(core, now);
    }

    fn remove_entity(&mut self, id: u32) {
        if id as usize >= MAX_ENTITIES || !self.entities[id as usize].used {
            return;
        }
        // Remove every key for this entity from every run queue. A key can
        // normally only exist on the entity's own core, but sweeping all cores
        // keeps a stale key from surviving retirement.
        let core_count = self.core_count();
        for core in 0..core_count {
            loop {
                let mut found = None;
                let tree = &self.cores[core].tree;
                tree.for_each(&mut |key| {
                    if key.id == id && found.is_none() {
                        found = Some(key);
                    }
                });
                match found {
                    Some(key) => {
                        let _ = self.cores[core].tree.remove(key);
                    }
                    None => break,
                }
            }
        }
        self.remove_timer(id);
        let entity = &mut self.entities[id as usize];
        entity.state = ST_EXITED;
        entity.remaining_ns = 0;
        for core in 0..core_count {
            if self.cores[core].current == id {
                self.cores[core].current = NO_ID;
            }
        }
        if self.current_user == id {
            self.current_user = NO_ID;
        }
    }

    #[cfg(debug_assertions)]
    fn check_invariants(&mut self) {
        self.checks = self.checks.saturating_add(1);
        let mut violations = 0u32;
        let core_count = self.core_count();
        let mut present = [0u8; MAX_ENTITIES];

        for core in 0..core_count {
            if self.cores[core].tree.validate().is_err() {
                violations += 1;
            }
            {
                let tree = &self.cores[core].tree;
                let entities = &self.entities;
                tree.for_each(&mut |key: Key| {
                    let id = key.id as usize;
                    if id >= MAX_ENTITIES || !entities[id].used {
                        violations += 1;
                        return;
                    }
                    present[id] = present[id].saturating_add(1);
                    if entities[id].core as usize != core {
                        violations += 1;
                    }
                    if entities[id].state != ST_RUNNABLE {
                        violations += 1;
                    }
                    if entities[id].deadline != key.deadline {
                        violations += 1;
                    }
                });
            }
        }

        for id in 0..MAX_ENTITIES {
            let entity = &self.entities[id];
            if !entity.used {
                continue;
            }
            match entity.state {
                ST_RUNNABLE => {
                    if present[id] != 1 {
                        violations += 1;
                    }
                    let core = entity.core as usize;
                    if core >= MAX_SMP_CORES
                        || core >= 16
                        || entity.affinity & (1u16 << core) == 0
                    {
                        violations += 1;
                    }
                }
                ST_RUNNING => {
                    if present[id] != 0 {
                        violations += 1;
                    }
                    let core = entity.core as usize;
                    if core >= MAX_SMP_CORES || self.cores[core].current != id as u32 {
                        violations += 1;
                    }
                    let mut running = 0u32;
                    for candidate in 0..core_count {
                        if self.cores[candidate].current == id as u32 {
                            running += 1;
                        }
                    }
                    if running != 1 {
                        violations += 1;
                    }
                    if core >= 16 || entity.affinity & (1u16 << core) == 0 {
                        violations += 1;
                    }
                }
                _ => {
                    if present[id] != 0 {
                        violations += 1;
                    }
                }
            }
        }

        if violations != 0 {
            self.violations = self.violations.saturating_add(violations as u64);
            println!(
                "Sched: INVARIANT VIOLATION: {} problem(s) at check {}",
                violations, self.checks
            );
        }
    }
}

static STATE: IrqSpinLock<State> = IrqSpinLock::new(State::EMPTY);

/// Prepare the entity table, worker stacks and the kernel demo run queue.
pub fn init() -> Result<(), &'static str> {
    if !gic::is_ready() {
        return Err("GIC is not initialized");
    }
    if !timer::is_ready() {
        return Err("generic timer is not initialized");
    }
    install_guard_pages()?;

    let core_count = topology::core_count().clamp(1, MAX_SMP_CORES);
    let all_cores = if core_count >= 16 {
        u16::MAX
    } else {
        (1u16 << core_count) - 1
    };
    let now = timer::now_ns();

    let mut state = STATE.lock();
    for worker in 0..KERNEL_WORKERS {
        let id = worker as u32;
        let frame = build_worker_frame(worker)?;
        // Every worker starts at the same virtual time and the same virtual
        // deadline for a full slice request. EEVDF breaks the tie by id, and
        // the first dispatch advances the winner's virtual time past the rest,
        // so the tree rotates through all of them. Staggering the initial
        // deadlines instead would leave the earliest entity with a rolling
        // deadline ahead of workers that never ran.
        let virtual_deadline = deadline(now, DEMO_SLICE_NS, DEMO_WEIGHT);
        state.entities[worker] = Entity {
            used: true,
            kind: KIND_KERNEL,
            state: ST_RUNNABLE,
            id,
            weight: DEMO_WEIGHT,
            slice_ns: DEMO_SLICE_NS,
            vtime: now,
            deadline: virtual_deadline,
            remaining_ns: DEMO_SLICE_NS,
            core: 0,
            affinity: all_cores,
            frame: frame as u64,
            stack_top: kernel_stack_top(worker),
            ttbr0: 0,
            pid: 0,
            tid: 0,
            wake_at_ns: 0,
            runnable_since_ns: now,
            last_start_ns: now,
            run_ns: 0,
            wake_latency: LatencyStats::new(),
            run_delay: LatencyStats::new(),
        };
        state.cores[0]
            .tree
            .insert(Key::new(virtual_deadline, id))
            .map_err(|_| "could not seed the boot run queue")?;
    }

    state.next_id = KERNEL_WORKERS as u32;
    state.demo_start_ns = now;
    state.demo_end_ns = now.saturating_add(DEMO_DURATION_NS);
    state.demo_active = true;
    state.ready = true;
    Ok(())
}

/// Remove every kernel and idle stack guard page from the kernel map.
fn install_guard_pages() -> Result<(), &'static str> {
    for id in 0..MAX_ENTITIES {
        mmu::unmap_page(kernel_stack_base(id))
            .map_err(|_| "could not unmap a kernel worker guard page")?;
    }
    for core in 0..MAX_SMP_CORES {
        mmu::unmap_page(idle_stack_base(core))
            .map_err(|_| "could not unmap an idle stack guard page")?;
    }
    Ok(())
}

fn build_worker_frame(worker: usize) -> Result<*mut TrapFrame, &'static str> {
    if worker >= KERNEL_WORKERS {
        return Err("kernel worker index is out of range");
    }
    let top = kernel_stack_top(worker);
    let address = top - size_of::<TrapFrame>() as u64;
    let frame = address as *mut TrapFrame;
    // SAFETY: the frame lies at the top of this worker's stack and is written
    // before the entity can run.
    unsafe {
        ptr::write_bytes(frame as *mut u8, 0, size_of::<TrapFrame>());
        (*frame).x[0] = worker as u64;
        (*frame).elr = worker_main as *const () as usize as u64;
        (*frame).spsr = SPSR_EL1H_IRQ_ON;
    }
    WORK[worker].store(0, Ordering::Relaxed);
    FP_BITS[worker].store(0, Ordering::Relaxed);
    Ok(frame)
}

/// Print the scheduler configuration.
pub fn report() {
    let cores = topology::core_count().clamp(1, MAX_SMP_CORES);
    let clusters = topology::cluster_count().max(1);
    println!(
        "Scheduler: EEVDF, {} kernel workers, {} KiB stacks with guard pages, {} us slice request, {}-ms demo",
        KERNEL_WORKERS,
        STACK_SIZE / 1024,
        DEMO_SLICE_NS / 1000,
        DEMO_DURATION_NS / 1_000_000
    );
    println!(
        "Sched: {} per-core run queues, one-shot CNTV events, {} cluster{}, cluster-aware stealing",
        cores,
        clusters,
        if clusters == 1 { "" } else { "s" }
    );
}

/// Start the boot core's scheduler. Does not return.
pub fn start() -> ! {
    gic::disable_irq();
    let core = percpu::current_index();
    let frame = {
        let mut state = STATE.lock();
        state.cores[core].ready = true;
        let now = timer::now_ns();
        let frame = match state.select_and_dispatch(core, now) {
            Some(frame) => frame,
            None => state.build_idle_frame(core),
        };
        state.arm_next_event(core, now);
        #[cfg(debug_assertions)]
        state.check_invariants();
        frame
    };
    enter_task(frame)
}

/// Start a secondary core's scheduler. Does not return.
pub fn start_core() -> ! {
    gic::disable_irq();
    let core = percpu::current_index();
    let frame = {
        let mut state = STATE.lock();
        state.cores[core].ready = true;
        let now = timer::now_ns();
        let frame = match state.select_and_dispatch(core, now) {
            Some(frame) => frame,
            None => state.build_idle_frame(core),
        };
        state.arm_next_event(core, now);
        #[cfg(debug_assertions)]
        state.check_invariants();
        frame
    };
    enter_task(frame)
}

/// Body of every kernel worker. `id` arrives in `x0` from the initial frame.
extern "C" fn worker_main(id: u64) -> ! {
    let worker = id as usize;
    if worker >= KERNEL_WORKERS {
        crate::arch::halt();
    }
    let mut accumulator = worker as f64 + 1.0;
    loop {
        WORK[worker].fetch_add(1, Ordering::Relaxed);
        // Attribute work to the core executing this iteration, not to the core
        // that ran the worker first: a stolen worker keeps running this loop
        // after it migrates, so a cached record would credit the wrong core and
        // the thief would look idle.
        percpu::current().work.fetch_add(1, Ordering::Relaxed);
        accumulator += 1.0;
        FP_BITS[worker].store(accumulator.to_bits(), Ordering::Relaxed);
    }
}

/// Idle body. It either steals work, or arms the next event and waits.
extern "C" fn idle_main(core: u64) -> ! {
    let core = core as usize;
    loop {
        let mut complete_user = false;
        let frame = {
            let mut state = STATE.lock();
            let now = timer::now_ns();
            if state.user_active && !state.user_work_left() {
                // Leave the lock before reporting: the process report takes the
                // process table and then the scheduler.
                complete_user = true;
                None
            } else if !state.user_active && state.demo_active && now >= state.demo_end_ns {
                Some(state.finish_core_frame(core))
            } else {
                state.wake_timers(now);
                match state.select_and_dispatch(core, now) {
                    Some(frame) => Some(frame),
                    None => {
                        state.arm_next_event(core, now);
                        None
                    }
                }
            }
        };

        if complete_user {
            crate::process::user_phase_complete();
        }

        if let Some(frame) = frame {
            enter_task(frame);
        }

        IDLE_MASK.fetch_or(1u16 << core, Ordering::Release);
        timer::wait_for_interrupt();
        IDLE_MASK.fetch_and(!(1u16 << core), Ordering::Release);
    }
}

/// Timer entry from `irq_handler`. Returns the frame to resume.
pub fn on_tick(frame: *mut TrapFrame) -> *mut TrapFrame {
    let core = percpu::current_index();
    if core >= MAX_SMP_CORES {
        return frame;
    }
    record_tick();

    let now = timer::now_ns();
    let mut state = STATE.lock();
    if !state.cores[core].ready {
        return frame;
    }
    state.cores[core].ticks = state.cores[core].ticks.saturating_add(1);

    if state.user_active {
        state.wake_timers(now);
        if state.cores[core].current == NO_ID {
            let next = match state.select_and_dispatch(core, now) {
                Some(frame) => frame,
                None => {
                    state.arm_next_event(core, now);
                    return frame;
                }
            };
            state.arm_next_event(core, now);
            #[cfg(debug_assertions)]
            state.check_invariants();
            return next;
        }
        if !state.should_preempt(core, now) {
            state.arm_next_event(core, now);
            return frame;
        }
    } else {
        if !state.demo_active {
            state.arm_next_event(core, now);
            return frame;
        }
        if now >= state.demo_end_ns {
            let next = state.finish_core_frame(core);
            state.arm_next_event(core, now);
            #[cfg(debug_assertions)]
            state.check_invariants();
            return next;
        }
        if !state.should_preempt(core, now) {
            state.arm_next_event(core, now);
            return frame;
        }
    }

    state.save_frame(core, frame);
    let next = match state.select_and_dispatch(core, now) {
        Some(frame) => frame,
        None => state.build_idle_frame(core),
    };
    state.arm_next_event(core, now);
    #[cfg(debug_assertions)]
    state.check_invariants();
    next
}

/// IRQ entry from `vectors.S`. Acknowledges one Group 1 interrupt and returns
/// the frame to resume, which may belong to a different entity.
///
/// `vectors.S` passes the vector index in `x0` and the trap frame in `x1` for
/// every entry, so the signature matches `exception_handler` even though the
/// index is not used here. Taking only one argument would read the index as a
/// frame pointer and fault on the return path.
#[no_mangle]
pub extern "C" fn irq_handler(_index: u64, frame: *mut TrapFrame) -> *mut TrapFrame {
    let intid = gic::acknowledge();
    if intid >= 1020 {
        // 1023 is spurious; 1020..=1022 are special INTIDs with no EOI.
        return frame;
    }

    let next = if intid == gic::timer_irq() {
        // Mask the level-sensitive PPI before doing any work so it cannot
        // re-assert while the handler still runs.
        timer::disarm();
        on_tick(frame)
    } else {
        match intid {
            gic::SGI_PING | gic::SGI_PONG => {
                smp::on_sgi(intid);
                frame
            }
            gic::SGI_RESCHED => on_resched(frame),
            gic::SGI_BALANCE => {
                on_balance(percpu::current_index());
                on_resched(frame)
            }
            _ => frame,
        }
    };

    gic::end(intid);
    next
}

/// `SGI_RESCHED`: re-evaluate this core without waiting for a timer.
pub fn on_resched(frame: *mut TrapFrame) -> *mut TrapFrame {
    let core = percpu::current_index();
    if core >= MAX_SMP_CORES {
        return frame;
    }
    let now = timer::now_ns();
    let mut state = STATE.lock();
    if !state.cores[core].ready {
        return frame;
    }

    if state.cores[core].current == NO_ID {
        let next = match state.select_and_dispatch(core, now) {
            Some(frame) => frame,
            None => frame,
        };
        state.arm_next_event(core, now);
        return next;
    }

    if !state.should_preempt(core, now) {
        state.arm_next_event(core, now);
        return frame;
    }

    state.save_frame(core, frame);
    let next = match state.select_and_dispatch(core, now) {
        Some(frame) => frame,
        None => state.build_idle_frame(core),
    };
    state.arm_next_event(core, now);
    #[cfg(debug_assertions)]
    state.check_invariants();
    next
}

/// `SGI_BALANCE`: push the latest-deadline entity to an idle core.
pub fn on_balance(core: usize) {
    if core >= MAX_SMP_CORES {
        return;
    }
    let now = timer::now_ns();
    let mut state = STATE.lock();
    let elapsed = now.saturating_sub(state.cores[core].balance_sent_ns);
    if elapsed < BALANCE_COOLDOWN_NS {
        return;
    }
    state.cores[core].balance_sent_ns = now;

    let idle = IDLE_MASK.load(Ordering::Acquire);
    let core_count = state.core_count();
    for target in 0..core_count {
        if target == core || idle & (1u16 << target) == 0 {
            continue;
        }
        if state.cores[target].current != NO_ID {
            continue;
        }
        let Some(key) = state.cores[core].tree.last() else {
            return;
        };
        let id = key.id;
        if id as usize >= MAX_ENTITIES {
            continue;
        }
        let entity = &state.entities[id as usize];
        if !entity.used || entity.state != ST_RUNNABLE {
            continue;
        }
        if entity.affinity & (1u16 << target) == 0 {
            continue;
        }
        if state.cores[core].tree.remove(key).is_err() {
            continue;
        }
        if state.cores[target].tree.insert(key).is_err() {
            let _ = state.cores[core].tree.insert(key);
            continue;
        }
        state.entities[id as usize].core = target as u8;
        state.migrations = state.migrations.saturating_add(1);
        if topology::same_cluster(core, target) {
            state.same_cluster_steals = state.same_cluster_steals.saturating_add(1);
        } else {
            state.cross_cluster_steals = state.cross_cluster_steals.saturating_add(1);
        }
        if let Some(record) = percpu::record(target) {
            let _ = gic::send_sgi(record.mpidr.load(Ordering::Relaxed), gic::SGI_RESCHED);
        }
        return;
    }
}

fn record_tick() {
    // Kept as a tiny helper so the per-core SMP report still shows timer
    // activity without every call site repeating the atomic sequence.
    percpu::current()
        .sched_ticks
        .fetch_add(1, Ordering::Relaxed);
}

/// End this core's kernel demo. Runs outside interrupt context.
extern "C" fn finish_task(core: u64) -> ! {
    let core = core as usize;
    if core >= MAX_SMP_CORES {
        crate::arch::halt();
    }

    let (ticks, switches, steals) = {
        let state = STATE.lock();
        let core_state = &state.cores[core];
        (core_state.ticks, core_state.switches, core_state.steals)
    };
    println!();
    println!(
        "Scheduler: core {} demo complete, {} ticks, {} context switches, {} steals",
        core, ticks, switches, steals
    );

    if core != percpu::boot_index() {
        smp::mark_sched_done();
        // This core's demo window is over. Park in the tickless idle path
        // rather than halting with a timer still armed: the empty run queue
        // disarms CNTV and the core waits for an interrupt with real work.
        gic::enable_irq();
        loop {
            {
                let mut state = STATE.lock();
                state.park_idle(core);
            }
            timer::wait_for_interrupt();
        }
    }

    let completed = smp::wait_for_schedulers();
    report_results(completed);

    println!("Scheduler: boot core handing off to the user phase");
    match crate::process::start_user_phase() {
        Ok(frame) => enter_task(frame),
        Err(reason) => crate::process::fail(reason),
    }
}

fn report_results(completed: bool) {
    let (migrations, same, cross, steals, balance, checks, violations, disarms, events) = {
        let state = STATE.lock();
        (
            state.migrations,
            state.same_cluster_steals,
            state.cross_cluster_steals,
            state.steals,
            state.balance_sgis,
            state.checks,
            state.violations,
            state.disarms,
            timer::armed_events(),
        )
    };

    let mut fp_ok = 0usize;
    for worker in 0..KERNEL_WORKERS {
        let iterations = WORK[worker].load(Ordering::Relaxed);
        let bits = FP_BITS[worker].load(Ordering::Relaxed);
        let value = f64::from_bits(bits);
        let expected = iterations as f64 + worker as f64 + 1.0;
        let matched = value == expected || value == expected - 1.0;
        if matched {
            fp_ok += 1;
        }
        println!(
            "  worker {}: {} iterations, fp bits 0x{:016x}, fp {}",
            worker,
            iterations,
            bits,
            if matched { "ok" } else { "MISMATCH" }
        );
    }

    println!(
        "Sched: {} steals, {} migrations ({} same-cluster, {} cross-cluster), {} balance SGIs",
        steals, migrations, same, cross, balance
    );
    println!(
        "Sched: one-shot timer: {} events armed, {} idle disarms (tickless), {} IRQ masks",
        events,
        disarms,
        timer::disarm_count()
    );
    #[cfg(debug_assertions)]
    println!(
        "Sched: invariant checker: {} checks, {} violations",
        checks, violations
    );
    #[cfg(not(debug_assertions))]
    {
        let _ = (checks, violations);
        println!("Sched: invariant checker: disabled in release build");
    }

    if fp_ok != KERNEL_WORKERS {
        println!(
            "Sched: WARNING {} of {} worker FP states did not match",
            KERNEL_WORKERS - fp_ok,
            KERNEL_WORKERS
        );
    }

    if completed && fp_ok == KERNEL_WORKERS && smp::concurrency_verified() {
        println!("SMP: concurrency verified (every online core executed scheduler work)");
    } else if smp::online_count() >= 2 {
        println!("SMP: WARNING concurrency was not verified");
    } else {
        println!("SMP: single-core fallback (no secondary online)");
    }
    // Per-core evidence, so a failure above can be attributed to a specific
    // core rather than guessed at.
    smp::report_results();
}

// ---------------------------------------------------------------------------
// User-thread interface used by the process layer.
// ---------------------------------------------------------------------------

/// Register one user thread as a scheduler entity pinned to the boot core.
pub fn register_user(
    pid: u32,
    tid: u32,
    frame: *mut TrapFrame,
    stack_top: u64,
    ttbr0: u64,
    weight: u32,
    slice_ns: u64,
) -> Result<u32, &'static str> {
    let core = percpu::boot_index().min(MAX_SMP_CORES - 1);
    let now = timer::now_ns();
    let mut state = STATE.lock();
    let id = state.next_id;
    if id as usize >= MAX_ENTITIES {
        return Err("scheduler entity table is full");
    }
    state.next_id += 1;

    let slice_ns = slice_ns.clamp(500_000, 50_000_000);
    let weight = weight.clamp(16, 65_535);
    // A new thread joins at the run queue's floor, not below it. Starting at
    // `now` would give the newcomer positive lag when the floor is behind the
    // wall clock, and `floor - LAG_LIMIT` would hand it a full burst of credit.
    let vtime = state.floor(core);
    state.entities[id as usize] = Entity {
        used: true,
        kind: KIND_USER,
        state: ST_RUNNABLE,
        id,
        weight,
        slice_ns,
        vtime,
        deadline: deadline(vtime, slice_ns, weight),
        remaining_ns: 0,
        core: core as u8,
        affinity: 1u16 << core,
        frame: frame as u64,
        stack_top,
        ttbr0,
        pid,
        tid,
        wake_at_ns: 0,
        runnable_since_ns: now,
        last_start_ns: now,
        run_ns: 0,
        wake_latency: LatencyStats::new(),
        run_delay: LatencyStats::new(),
    };
    state.requeue(id, now);
    #[cfg(debug_assertions)]
    state.check_invariants();
    Ok(id)
}

/// Mark every entity belonging to `pid` as exited.
pub fn exit_pid(pid: u32) {
    let mut state = STATE.lock();
    for id in 0..MAX_ENTITIES {
        let entity = state.entities[id];
        if entity.used && entity.kind == KIND_USER && entity.pid == pid {
            let id = entity.id;
            state.remove_entity(id);
        }
    }
    #[cfg(debug_assertions)]
    state.check_invariants();
}

/// Wake a blocked user thread and deliver its syscall result.
pub fn wake_user(tid: u32, result: i64) {
    let now = timer::now_ns();
    let mut state = STATE.lock();
    for id in KERNEL_WORKERS..MAX_ENTITIES {
        let entity = state.entities[id];
        if !entity.used || entity.kind != KIND_USER || entity.tid != tid {
            continue;
        }
        if entity.state != ST_BLOCKED {
            return;
        }
        if entity.frame != 0 {
            // SAFETY: the frame is the blocked thread's live trap frame on its
            // kernel stack, and only the scheduler writes it while blocked.
            unsafe {
                (*(entity.frame as *mut TrapFrame)).x[0] = result as u64;
            }
        }
        state.entities[id].state = ST_RUNNABLE;
        state.entities[id].remaining_ns = 0;
        state.requeue(id as u32, now);
        #[cfg(debug_assertions)]
        state.check_invariants();
        return;
    }
}

/// Start the user phase on the boot core and return its first frame.
pub fn enter_user_phase() -> Result<*mut TrapFrame, &'static str> {
    let core = percpu::boot_index().min(MAX_SMP_CORES - 1);
    let now = timer::now_ns();
    let mut state = STATE.lock();
    state.user_active = true;
    state.end_demo();
    state.current_user = NO_ID;
    let frame = state
        .select_and_dispatch(core, now)
        .ok_or("no runnable user thread was registered")?;
    state.arm_next_event(core, now);
    #[cfg(debug_assertions)]
    state.check_invariants();
    Ok(frame)
}

/// Current user `(pid, tid)`, or `None` while a kernel entity runs.
pub fn current_user() -> Option<(u32, u32)> {
    let state = STATE.lock();
    let id = state.current_user;
    if id == NO_ID || id as usize >= MAX_ENTITIES {
        return None;
    }
    let entity = &state.entities[id as usize];
    if !entity.used || entity.kind != KIND_USER {
        return None;
    }
    Some((entity.pid, entity.tid))
}

/// Block the current user thread. The caller has already recorded why.
pub fn user_block(frame: *mut TrapFrame) -> *mut TrapFrame {
    let core = percpu::current_index().min(MAX_SMP_CORES - 1);
    let now = timer::now_ns();
    let mut state = STATE.lock();
    let current = state.cores[core].current;
    if current != NO_ID {
        state.entities[current as usize].frame = frame as u64;
        state.charge(current, now);
        state.entities[current as usize].state = ST_BLOCKED;
        state.cores[core].current = NO_ID;
        if state.current_user == current {
            state.current_user = NO_ID;
        }
    }
    let next = match state.select_and_dispatch(core, now) {
        Some(frame) => frame,
        None => {
            if state.user_work_left() {
                state.build_idle_frame(core)
            } else {
                state.finish_user_frame(frame)
            }
        }
    };
    state.arm_next_event(core, now);
    #[cfg(debug_assertions)]
    state.check_invariants();
    next
}

/// Sleep the current user thread for `duration_ns`.
pub fn user_sleep(frame: *mut TrapFrame, duration_ns: u64) -> *mut TrapFrame {
    if duration_ns == 0 {
        return user_yield(frame);
    }
    let core = percpu::current_index().min(MAX_SMP_CORES - 1);
    let now = timer::now_ns();
    let duration_ns = duration_ns.min(10_000_000_000);
    let wake_at = now.saturating_add(duration_ns);
    let mut state = STATE.lock();
    let current = state.cores[core].current;
    if current != NO_ID {
        state.entities[current as usize].frame = frame as u64;
        state.charge(current, now);
        state.entities[current as usize].state = ST_SLEEPING;
        state.entities[current as usize].wake_at_ns = wake_at;
        if !state.add_timer(current, wake_at) {
            // Degrade to a plain yield rather than silently sleeping forever.
            state.entities[current as usize].state = ST_RUNNABLE;
            state.entities[current as usize].wake_at_ns = 0;
            state.requeue(current, now);
        }
        state.cores[core].current = NO_ID;
        if state.current_user == current {
            state.current_user = NO_ID;
        }
    }
    let next = match state.select_and_dispatch(core, now) {
        Some(frame) => frame,
        None => {
            if state.user_work_left() {
                state.build_idle_frame(core)
            } else {
                state.finish_user_frame(frame)
            }
        }
    };
    state.arm_next_event(core, now);
    #[cfg(debug_assertions)]
    state.check_invariants();
    next
}

/// Yield the rest of the requested slice.
pub fn user_yield(frame: *mut TrapFrame) -> *mut TrapFrame {
    let core = percpu::current_index().min(MAX_SMP_CORES - 1);
    let now = timer::now_ns();
    let mut state = STATE.lock();
    let current = state.cores[core].current;
    if current != NO_ID {
        state.entities[current as usize].frame = frame as u64;
        state.charge_yield(current, now);
        state.cores[core].current = NO_ID;
        if state.current_user == current {
            state.current_user = NO_ID;
        }
        state.requeue(current, now);
    }
    let next = match state.select_and_dispatch(core, now) {
        Some(frame) => frame,
        None => {
            if state.user_work_left() {
                state.build_idle_frame(core)
            } else {
                state.finish_user_frame(frame)
            }
        }
    };
    state.arm_next_event(core, now);
    #[cfg(debug_assertions)]
    state.check_invariants();
    next
}

/// Exit the current user thread and pick the next one.
pub fn user_exit(frame: *mut TrapFrame) -> *mut TrapFrame {
    let core = percpu::current_index().min(MAX_SMP_CORES - 1);
    let now = timer::now_ns();
    let mut state = STATE.lock();
    let current = state.cores[core].current;
    if current != NO_ID {
        let entity = &mut state.entities[current as usize];
        entity.frame = frame as u64;
        if entity.state == ST_RUNNING {
            entity.state = ST_EXITED;
        }
        state.cores[core].current = NO_ID;
        if state.current_user == current {
            state.current_user = NO_ID;
        }
    }
    let next = match state.select_and_dispatch(core, now) {
        Some(frame) => frame,
        None => {
            if state.user_work_left() {
                state.build_idle_frame(core)
            } else {
                state.finish_user_frame(frame)
            }
        }
    };
    state.arm_next_event(core, now);
    #[cfg(debug_assertions)]
    state.check_invariants();
    next
}

/// Report per-thread wake latency and run delay. Values are bucket upper
/// bounds; the label names QEMU-TCG because verification runs under emulation.
pub fn report_latency() {
    let state = STATE.lock();
    let mut any = false;
    for entity in &state.entities {
        if !entity.used || entity.kind != KIND_USER {
            continue;
        }
        if entity.wake_latency.count() == 0 && entity.run_delay.count() == 0 {
            continue;
        }
        any = true;
        println!(
            "  pid {} tid {}: wake n {}, p50 <= {} ns, p99 <= {} ns, max {} ns; run delay n {}, p50 <= {} ns, p99 <= {} ns",
            entity.pid,
            entity.tid,
            entity.wake_latency.count(),
            entity.wake_latency.percentile_upper_ns(50),
            entity.wake_latency.percentile_upper_ns(99),
            entity.wake_latency.max_ns(),
            entity.run_delay.count(),
            entity.run_delay.percentile_upper_ns(50),
            entity.run_delay.percentile_upper_ns(99),
        );
    }
    if any {
        println!("Sched: latency figures are QEMU-TCG diagnostics, not performance claims");
    } else {
        println!("Sched: no user latency samples were recorded");
    }
}

/// Summary usable by the final process report.
#[allow(dead_code)]
pub fn entity_count() -> usize {
    let state = STATE.lock();
    state.entities.iter().filter(|entity| entity.used).count()
}
