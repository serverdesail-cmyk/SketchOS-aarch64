//! Process and thread table, EL0 entry, user page ownership and IPC.
//!
//! # Ownership
//!
//! A [`Process`] owns one user [`AddressSpace`] and the physical pages mapped
//! into it. A [`Thread`] owns one kernel stack and the trap frame saved at its
//! top. The boot core runs the user phase after the 0.0.4 SMP/scheduler demo;
//! secondaries are parked by then.
//!
//! # Lifecycle
//!
//! `start_user_phase` loads every embedded image, creates one or two threads
//! per process, and enters the first thread through `scheduler_enter`. A thread
//! runs until it calls `exit`, blocks in `recv`, or faults. `exit` and a user
//! fault both reap the whole process: its pages are returned to the physical
//! allocator, its threads become `Exited`, and blocked peers are woken with
//! `-ESRCH`. When no thread is runnable, a kernel frame runs
//! [`user_phase_complete`], which prints the result and powers the machine off.
//!
//! Page tables themselves are not reclaimed. They live in the kernel's fixed
//! table pool and are abandoned with the address space; 0.0.5 documents that
//! limitation instead of pretending to free them.
//!
//! # Scheduling
//!
//! User threads are scheduler entities owned by [`crate::sched`]. The process
//! layer keeps the names, address spaces and IPC state; the scheduler owns the
//! run queues, slice requests and dispatch. All user threads stay pinned to the
//! boot core in 0.0.6, because cross-core process teardown would need a TLB
//! shootdown and a quiesce protocol this milestone does not implement. The
//! entity affinity masks already carry the boot core bit so a later milestone
//! can migrate them without changing this handoff.
//!
//! The lock order is `TABLE -> scheduler`. Every syscall resolves the running
//! thread through the scheduler while the process table is held; no scheduler
//! path ever takes the process-table lock.

use core::cell::UnsafeCell;
use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use sketchos_bootinfo::{phys_to_virt, PAGE_SIZE};
use sketchos_elf::Image;
use sketchos_mm::pagetable::Perm;

use crate::arch::aarch64::{
    self, cache, gic, mmu::AddressSpace, set_user_fault_hook, FaultAction, FaultInfo, TrapFrame,
};
use crate::sync::IrqSpinLock;
use crate::{percpu, phys, println, sched, smp};
#[cfg(feature = "shell")]
use crate::shell;

pub const MAX_PROCESSES: usize = 8;
pub const MAX_THREADS: usize = 16;
pub const MAX_PAGES_PER_PROCESS: usize = 64;
pub const MAX_MESSAGE: usize = 64;
const MAX_THREADS_PER_PROCESS: usize = 2;
const USER_STACK_PAGES: usize = 4;
const USER_STACK_TOP: u64 = 0x0000_0000_8000_0000;
const INITIAL_STACK_BYTES: u64 = 48;
const THREAD_STACK_SIZE: usize = 16 * 1024;
const GUARD_SIZE: usize = 4096;

/// Default scheduling class for a user thread.
const USER_WEIGHT: u32 = 1024;
const USER_SLICE_NS: u64 = 4_000_000;
/// The latency workload's short-slice sleeper: high weight, tiny request.
const LATENCY_SLEEPER_WEIGHT: u32 = 4096;
const LATENCY_SLEEPER_SLICE_NS: u64 = 1_000_000;
/// The latency workload's CPU hog: low weight, long request.
const LATENCY_HOG_WEIGHT: u32 = 256;
const LATENCY_HOG_SLICE_NS: u64 = 20_000_000;
/// The demo latency process.
const LATENCY_PID: usize = 6;

const EINVAL: i64 = -22;
const ESRCH: i64 = -3;
const EAGAIN: i64 = -11;
const EFAULT: i64 = -14;
const EMSGSIZE: i64 = -90;

const INIT_IMAGE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../userspace/build/init.elf"
));
const IPC_RECV_IMAGE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../userspace/build/ipc-recv.elf"
));
const IPC_SEND_IMAGE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../userspace/build/ipc-send.elf"
));
const FAULTER_IMAGE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../userspace/build/faulter.elf"
));
const THREADS_IMAGE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../userspace/build/threads.elf"
));
const LATENCY_IMAGE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../userspace/build/latency.elf"
));

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProcessState {
    Unused,
    Live,
    Zombie,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ThreadState {
    Unused,
    Runnable,
    Running,
    Blocked,
    Exited,
}

#[derive(Clone, Copy)]
struct Message {
    from_pid: u64,
    length: usize,
    data: [u8; MAX_MESSAGE],
    sender_thread: usize,
}

#[derive(Clone, Copy)]
struct Ipc {
    recv_waiting: bool,
    recv_from: u64,
    recv_buffer: u64,
    recv_capacity: u64,
    pending: Option<Message>,
}

impl Ipc {
    const EMPTY: Self = Self {
        recv_waiting: false,
        recv_from: 0,
        recv_buffer: 0,
        recv_capacity: 0,
        pending: None,
    };
}

struct Process {
    state: ProcessState,
    pid: u64,
    space: Option<AddressSpace>,
    pages: [u64; MAX_PAGES_PER_PROCESS],
    page_count: usize,
    threads: [Option<usize>; MAX_THREADS_PER_PROCESS],
    thread_count: usize,
    exit_status: u64,
    faulted: bool,
    ipc: Ipc,
}

impl Process {
    const EMPTY: Self = Self {
        state: ProcessState::Unused,
        pid: 0,
        space: None,
        pages: [0; MAX_PAGES_PER_PROCESS],
        page_count: 0,
        threads: [None; MAX_THREADS_PER_PROCESS],
        thread_count: 0,
        exit_status: 0,
        faulted: false,
        ipc: Ipc::EMPTY,
    };
}

struct Thread {
    state: ThreadState,
    tid: usize,
    pid: usize,
    entry: u64,
    arg0: u64,
    arg1: u64,
    frame: u64,
    stack_top: u64,
    user_stack_top: u64,
    /// Scheduler entity id, valid once the thread is registered.
    entity: u32,
}

impl Thread {
    const EMPTY: Self = Self {
        state: ThreadState::Unused,
        tid: 0,
        pid: 0,
        entry: 0,
        arg0: 0,
        arg1: 0,
        frame: 0,
        stack_top: 0,
        user_stack_top: 0,
        entity: u32::MAX,
    };
}

#[repr(C, align(4096))]
struct ThreadStack {
    _guard: [u8; GUARD_SIZE],
    data: [u8; THREAD_STACK_SIZE],
}

struct StackSet(UnsafeCell<[ThreadStack; MAX_THREADS]>);

// SAFETY: a thread stack is written only by the boot core while the owning
// thread is not running, and the user phase runs on the boot core alone.
unsafe impl Sync for StackSet {}

static THREAD_STACKS: StackSet = StackSet(UnsafeCell::new([const {
    ThreadStack {
        _guard: [0; GUARD_SIZE],
        data: [0; THREAD_STACK_SIZE],
    }
}; MAX_THREADS]));

struct Table {
    processes: [Process; MAX_PROCESSES],
    threads: [Thread; MAX_THREADS],
    current: usize,
    live_processes: usize,
    live_threads: usize,
}

impl Table {
    const EMPTY: Self = Self {
        processes: [const { Process::EMPTY }; MAX_PROCESSES],
        threads: [const { Thread::EMPTY }; MAX_THREADS],
        current: MAX_THREADS,
        live_processes: 0,
        live_threads: 0,
    };
}

static TABLE: IrqSpinLock<Table> = IrqSpinLock::new(Table::EMPTY);
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// True once the boot core has handed control to EL0.
pub fn user_phase_active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

fn stack_base(tid: usize) -> u64 {
    debug_assert!(tid < MAX_THREADS);
    // SAFETY: the boot core owns the whole stack array during the user phase.
    let stacks = unsafe { &mut *THREAD_STACKS.0.get() };
    stacks[tid]._guard.as_mut_ptr() as u64
}

fn stack_top(tid: usize) -> u64 {
    stack_base(tid) + size_of::<ThreadStack>() as u64
}

fn install_guard_pages() -> Result<(), &'static str> {
    for tid in 0..MAX_THREADS {
        mmu_unmap_guard(stack_base(tid))?;
    }
    Ok(())
}

fn mmu_unmap_guard(address: u64) -> Result<(), &'static str> {
    crate::arch::aarch64::mmu::unmap_page(address)
        .map_err(|_| "could not unmap a user-thread stack guard page")
}

/// Start the user phase on the boot core. Returns the first trap frame.
///
/// The caller enters it with `enter_task`. On error the caller must call
/// [`finish`] instead of pretending the phase ran.
pub fn start_user_phase() -> Result<*mut TrapFrame, &'static str> {
    if percpu::current_index() != percpu::boot_index() {
        return Err("the user phase must start on the boot core");
    }
    if ACTIVE.swap(true, Ordering::AcqRel) {
        return Err("the user phase is already active");
    }

    crate::arch::aarch64::mmu::unseal_low_half();
    install_guard_pages()?;
    // SAFETY: this function runs on the boot core after the kernel demo, and
    // the hook only records a decision from exception context.
    unsafe { set_user_fault_hook(Some(user_fault)) };

    let mut table = TABLE.lock();
    spawn_image(&mut table, 1, INIT_IMAGE, 0, 0, 0)?;
    spawn_image(&mut table, 2, IPC_RECV_IMAGE, 3, 0, 0)?;
    spawn_image(&mut table, 3, IPC_SEND_IMAGE, 2, 0, 0)?;
    spawn_image(&mut table, 4, FAULTER_IMAGE, 0, 0, 0)?;
    spawn_image(&mut table, 5, THREADS_IMAGE, 0, 0, 1)?;
    spawn_image(&mut table, LATENCY_PID, LATENCY_IMAGE, 0, 0, 1)?;
    register_threads(&mut table)?;

    println!(
        "Userspace: {} processes, {} threads, {} pages, {} KiB user stacks",
        table.live_processes,
        table.live_threads,
        table
            .processes
            .iter()
            .map(|process| process.page_count)
            .sum::<usize>(),
        USER_STACK_PAGES * 4
    );
    drop(table);

    let frame = sched::enter_user_phase()?;
    cache::invalidate_icache_all();
    // Keep the timer from interrupting the last few instructions before the
    // first `eret`; the restored SPSR turns IRQs back on at EL0.
    gic::disable_irq();
    Ok(frame)
}

/// Scheduling class for one thread of a process.
fn scheduling_class(pid: usize, thread_index: usize) -> (u32, u64) {
    if pid == LATENCY_PID {
        if thread_index == 0 {
            (LATENCY_SLEEPER_WEIGHT, LATENCY_SLEEPER_SLICE_NS)
        } else {
            (LATENCY_HOG_WEIGHT, LATENCY_HOG_SLICE_NS)
        }
    } else {
        (USER_WEIGHT, USER_SLICE_NS)
    }
}

/// Build each thread's initial frame and hand the runnable set to the
/// scheduler. Called with the process table locked, which is the outer side of
/// the `TABLE -> scheduler` lock order.
fn register_threads(table: &mut Table) -> Result<(), &'static str> {
    for pid in 1..MAX_PROCESSES {
        if table.processes[pid].state != ProcessState::Live {
            continue;
        }
        let ttbr0 = table.processes[pid]
            .space
            .as_ref()
            .map(|space| space.ttbr0_value())
            .ok_or("live process has no address space")?;
        let thread_count = table.processes[pid].thread_count;
        for index in 0..thread_count {
            let Some(tid) = table.processes[pid].threads[index] else {
                continue;
            };
            if table.threads[tid].state != ThreadState::Runnable {
                continue;
            }
            let (weight, slice_ns) = scheduling_class(pid, index);
            let frame = frame_for(table, tid);
            let stack_top = table.threads[tid].stack_top;
            let entity = sched::register_user(
                pid as u32,
                tid as u32,
                frame,
                stack_top,
                ttbr0,
                weight,
                slice_ns,
            )?;
            table.threads[tid].entity = entity;
        }
    }
    Ok(())
}

/// Resolve the running thread from the scheduler while the table is locked.
fn sync_current(table: &mut Table) {
    table.current = match sched::current_user() {
        Some((_, tid)) => tid as usize,
        None => MAX_THREADS,
    };
}

/// The 0.0.5 lower-EL fault hook. Every unrecoverable fault terminates only
/// the faulting process; the actual reap happens in [`terminate_current`].
pub fn user_fault(_info: &FaultInfo) -> FaultAction {
    FaultAction::Terminate
}

fn spawn_image(
    table: &mut Table,
    pid: usize,
    bytes: &[u8],
    arg0: u64,
    arg1: u64,
    extra_threads: usize,
) -> Result<usize, &'static str> {
    if pid == 0 || pid >= MAX_PROCESSES {
        return Err("user process id is out of range");
    }
    if table.processes[pid].state != ProcessState::Unused {
        return Err("user process slot is already in use");
    }
    if extra_threads + 1 > MAX_THREADS_PER_PROCESS {
        return Err("user process requests too many threads");
    }
    let thread_count = 1 + extra_threads;

    let image = Image::parse(bytes).map_err(|error| error.message())?;
    let mut image_start = u64::MAX;
    let mut image_end = 0u64;
    for segment in image.segments() {
        image_start = image_start.min(segment.virtual_address & !(PAGE_SIZE - 1));
        image_end = image_end.max(round_up(segment.end(), PAGE_SIZE));
    }
    if image_start < PAGE_SIZE || image_end <= image_start {
        return Err("user image has an invalid address range");
    }
    let stack_pages = USER_STACK_PAGES * thread_count;
    let stack_bottom = USER_STACK_TOP - stack_pages as u64 * PAGE_SIZE;
    if image_end > stack_bottom {
        return Err("user image overlaps the user stack");
    }
    let image_pages = ((image_end - image_start) / PAGE_SIZE) as usize;
    if image_pages == 0 || image_pages + stack_pages > MAX_PAGES_PER_PROCESS {
        return Err("user image exceeds the per-process page limit");
    }

    let mut space = AddressSpace::new_user().map_err(|error| error)?;
    let mut allocated = [0u64; MAX_PAGES_PER_PROCESS];
    let mut allocated_count = 0usize;
    let mut permissions = [0u8; MAX_PAGES_PER_PROCESS];

    let result = (|| -> Result<(), &'static str> {
        for index in 0..image_pages {
            let virtual_address = image_start + index as u64 * PAGE_SIZE;
            let page_end = virtual_address + PAGE_SIZE;
            let mut flags = 0u32;
            for segment in image.segments() {
                if segment.virtual_address < page_end && segment.end() > virtual_address {
                    flags |= segment.flags;
                }
            }
            let writable = flags & 2 != 0;
            let executable = flags & 1 != 0;
            if writable && executable {
                return Err("user image page is writable and executable");
            }
            permissions[index] = if executable {
                1
            } else if writable {
                2
            } else {
                0
            };
            let perm = Perm {
                read: true,
                write: writable,
                exec: executable,
                user: true,
                device: false,
                normal_nc: false,
                global: false,
            };

            let physical = phys::alloc_pages(1, PAGE_SIZE)
                .ok_or("user image: out of physical pages")?;
            allocated[allocated_count] = physical;
            allocated_count += 1;
            // SAFETY: the allocator returned one managed page and the physmap
            // makes it writable through a Normal mapping.
            unsafe {
                ptr::write_bytes(phys_to_virt(physical) as *mut u8, 0, PAGE_SIZE as usize);
            }
            copy_image_bytes(&image, bytes, virtual_address, physical);
            cache::clean_dcache_range(
                phys_to_virt(physical),
                phys_to_virt(physical) + PAGE_SIZE,
            );
            space
                .map(virtual_address, physical, PAGE_SIZE, perm)
                .map_err(|error| error)?;
        }

        let entry = image.entry();
        if !(image_start..image_end).contains(&entry) {
            return Err("user entry point is outside the loaded image");
        }
        let entry_page = ((entry - image_start) / PAGE_SIZE) as usize;
        if permissions.get(entry_page).copied().unwrap_or(0) != 1 {
            return Err("user entry point is not in executable memory");
        }

        let stack_start = allocated_count;
        for index in 0..stack_pages {
            let virtual_address = stack_bottom + index as u64 * PAGE_SIZE;
            let physical = phys::alloc_pages(1, PAGE_SIZE)
                .ok_or("user stack: out of physical pages")?;
            allocated[allocated_count] = physical;
            allocated_count += 1;
            // SAFETY: see the image page above.
            unsafe {
                ptr::write_bytes(phys_to_virt(physical) as *mut u8, 0, PAGE_SIZE as usize);
            }
            space
                .map(virtual_address, physical, PAGE_SIZE, Perm::USER_RW)
                .map_err(|error| error)?;
        }

        // Every thread gets its own user stack, all carved from the mapped
        // range. The initial record is argc = 0, argv[0] = NULL, envp[0] = NULL,
        // an AT_NULL pair and padding. It was zeroed with its page; writing it
        // again keeps the layout explicit.
        for thread in 0..thread_count {
            let record = USER_STACK_TOP
                - (thread as u64 * USER_STACK_PAGES as u64 * PAGE_SIZE)
                - INITIAL_STACK_BYTES;
            let page_index = ((record - stack_bottom) / PAGE_SIZE) as usize;
            let physical = allocated[stack_start + page_index];
            let offset = record & (PAGE_SIZE - 1);
            // SAFETY: the record lies inside a freshly allocated, mapped stack
            // page and one page of physical memory backs it.
            unsafe {
                ptr::write_bytes(
                    phys_to_virt(physical + offset) as *mut u8,
                    0,
                    INITIAL_STACK_BYTES as usize,
                );
            }
        }
        Ok(())
    })();

    if let Err(reason) = result {
        for page in &allocated[..allocated_count] {
            let _ = phys::free_pages(*page, 1);
        }
        space.destroy();
        return Err(reason);
    }

    let mut thread_ids = [usize::MAX; MAX_THREADS_PER_PROCESS];
    let mut found = 0usize;
    for slot in 0..table.threads.len() {
        if found == thread_count {
            break;
        }
        if table.threads[slot].state == ThreadState::Unused {
            thread_ids[found] = slot;
            found += 1;
        }
    }
    if found != thread_count {
        for page in &allocated[..allocated_count] {
            let _ = phys::free_pages(*page, 1);
        }
        space.destroy();
        return Err("out of user thread slots");
    }

    let mut process = Process::EMPTY;
    process.state = ProcessState::Live;
    process.pid = pid as u64;
    process.space = Some(space);
    process.pages = allocated;
    process.page_count = allocated_count;
    for thread in 0..thread_count {
        let tid = thread_ids[thread];
        process.threads[thread] = Some(tid);
        table.threads[tid] = Thread {
            state: ThreadState::Runnable,
            tid,
            pid,
            entry: image.entry(),
            arg0: if thread == 0 { arg0 } else { thread as u64 },
            arg1,
            frame: 0,
            stack_top: stack_top(tid),
            user_stack_top: USER_STACK_TOP
                - thread as u64 * USER_STACK_PAGES as u64 * PAGE_SIZE,
            entity: u32::MAX,
        };
    }
    process.thread_count = thread_count;
    table.processes[pid] = process;
    table.live_processes += 1;
    table.live_threads += thread_count;
    Ok(thread_ids[0])
}

fn copy_image_bytes(image: &Image, bytes: &[u8], page: u64, physical: u64) {
    let page_end = page + PAGE_SIZE;
    for segment in image.segments() {
        let file_end = segment.virtual_address + segment.file_size;
        let start = page.max(segment.virtual_address);
        let end = page_end.min(file_end);
        if end <= start {
            continue;
        }
        let source_start = (segment.offset + (start - segment.virtual_address)) as usize;
        let length = (end - start) as usize;
        let destination = phys_to_virt(physical + (start - page)) as *mut u8;
        // SAFETY: the parser validated `offset + file_size` against the image
        // slice, and the destination lies inside the newly allocated page.
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr().add(source_start), destination, length);
        }
    }
}

fn frame_for(table: &mut Table, tid: usize) -> *mut TrapFrame {
    let thread = &mut table.threads[tid];
    if thread.frame != 0 {
        return thread.frame as *mut TrapFrame;
    }
    let address = thread.stack_top - size_of::<TrapFrame>() as u64;
    let frame = address as *mut TrapFrame;
    // SAFETY: the frame fits in the thread's kernel stack, which nobody else
    // is using until this thread is entered.
    unsafe {
        ptr::write_bytes(frame as *mut u8, 0, size_of::<TrapFrame>());
        (*frame).x[0] = thread.arg0;
        (*frame).x[1] = thread.arg1;
        (*frame).elr = thread.entry;
        // EL0t, AArch64, all interrupts unmasked.
        (*frame).spsr = 0;
        (*frame).sp_el0 = thread.user_stack_top - INITIAL_STACK_BYTES;
    }
    thread.frame = address;
    frame
}

fn ret(frame: *mut TrapFrame, value: i64) -> *mut TrapFrame {
    // SAFETY: the caller owns the live trap frame from the exception path.
    unsafe {
        (*frame).x[0] = value as u64;
    }
    frame
}

/// Record that the current thread blocked in IPC and ask the scheduler to pick
/// the next runnable entity.
fn block_current(table: &mut Table, frame: *mut TrapFrame) -> *mut TrapFrame {
    let tid = table.current;
    if tid >= MAX_THREADS {
        return frame;
    }
    table.threads[tid].frame = frame as u64;
    table.threads[tid].state = ThreadState::Blocked;
    sched::user_block(frame)
}

fn wake_thread(table: &mut Table, tid: usize, result: i64) {
    if tid >= MAX_THREADS {
        return;
    }
    if table.threads[tid].state != ThreadState::Blocked {
        return;
    }
    table.threads[tid].state = ThreadState::Runnable;
    // The scheduler writes the result into the blocked thread's live frame and
    // puts the entity back on its run queue.
    sched::wake_user(tid as u32, result);
}

fn blocked_thread_of(table: &Table, pid: usize) -> Option<usize> {
    for thread in &table.threads {
        if thread.pid == pid && thread.state == ThreadState::Blocked {
            return Some(thread.tid);
        }
    }
    None
}

fn kill_process(table: &mut Table, pid: usize, status: u64, faulted: bool) {
    if pid == 0 || pid >= MAX_PROCESSES {
        return;
    }
    if table.processes[pid].state != ProcessState::Live {
        return;
    }

    let thread_ids = table.processes[pid].threads;
    let thread_count = table.processes[pid].thread_count;

    // Wake anything waiting for this process before its threads disappear.
    for other in 0..MAX_PROCESSES {
        if other == pid || other == 0 {
            continue;
        }
        if table.processes[other].state != ProcessState::Live {
            continue;
        }
        if table.processes[other].ipc.recv_waiting {
            let wanted = table.processes[other].ipc.recv_from;
            if wanted == 0 || wanted == pid as u64 {
                table.processes[other].ipc.recv_waiting = false;
                if let Some(tid) = blocked_thread_of(table, other) {
                    wake_thread(table, tid, ESRCH);
                }
            }
        }
        if table.processes[other]
            .ipc
            .pending
            .map(|message| message.from_pid == pid as u64)
            .unwrap_or(false)
        {
            table.processes[other].ipc.pending = None;
        }
    }

    if let Some(message) = table.processes[pid].ipc.pending {
        wake_thread(table, message.sender_thread, ESRCH);
    }
    table.processes[pid].ipc = Ipc::EMPTY;

    for index in 0..thread_count {
        if let Some(tid) = thread_ids[index] {
            if table.threads[tid].state != ThreadState::Exited {
                table.threads[tid].state = ThreadState::Exited;
                table.live_threads = table.live_threads.saturating_sub(1);
            }
        }
    }

    for index in 0..table.processes[pid].page_count {
        let physical = table.processes[pid].pages[index];
        if physical != 0 {
            let _ = phys::free_pages(physical, 1);
        }
    }
    table.processes[pid].page_count = 0;
    if let Some(space) = table.processes[pid].space.take() {
        space.destroy();
    }
    table.processes[pid].state = ProcessState::Zombie;
    table.processes[pid].exit_status = status;
    table.processes[pid].faulted = faulted;
    table.live_processes = table.live_processes.saturating_sub(1);
    if table.current < MAX_THREADS && table.threads[table.current].pid == pid {
        table.current = MAX_THREADS;
    }
}

/// Handle the `exit` syscall. Never returns to the caller.
pub fn exit_current(frame: *mut TrapFrame, status: u64) -> *mut TrapFrame {
    let mut table = TABLE.lock();
    sync_current(&mut table);
    let tid = table.current;
    if tid >= MAX_THREADS {
        return frame;
    }
    let pid = table.threads[tid].pid;
    println!("Userspace: pid {} requested exit({})", table.processes[pid].pid, status);
    kill_process(&mut table, pid, status, false);
    drop(table);
    sched::exit_pid(pid as u32);
    sched::user_exit(frame)
}

/// Handle a lower-EL synchronous fault that the hook could not recover.
pub fn terminate_current(frame: *mut TrapFrame) -> *mut TrapFrame {
    let mut table = TABLE.lock();
    sync_current(&mut table);
    let tid = table.current;
    if tid >= MAX_THREADS {
        return frame;
    }
    let pid = table.threads[tid].pid;
    // SAFETY: the frame is the faulting lower-EL frame.
    let (esr, far) = unsafe { ((*frame).esr, (*frame).far) };
    println!(
        "Userspace: pid {} fault (ESR 0x{:016x}, FAR 0x{:016x}), terminating process",
        table.processes[pid].pid,
        esr,
        far
    );
    kill_process(&mut table, pid, 139, true);
    drop(table);
    sched::exit_pid(pid as u32);
    sched::user_exit(frame)
}

/// Handle the `yield` syscall.
pub fn yield_current(frame: *mut TrapFrame) -> *mut TrapFrame {
    let mut table = TABLE.lock();
    sync_current(&mut table);
    let tid = table.current;
    if tid >= MAX_THREADS {
        return frame;
    }
    table.threads[tid].frame = frame as u64;
    // `yield` has no result; a resumed thread must not inherit whatever was in
    // x0 when it trapped.
    ret(frame, 0);
    drop(table);
    sched::user_yield(frame)
}

/// Handle the `sleep` syscall: block for `duration_ns` of wall time.
pub fn sleep_current(frame: *mut TrapFrame, duration_ns: u64) -> *mut TrapFrame {
    let mut table = TABLE.lock();
    sync_current(&mut table);
    let tid = table.current;
    if tid >= MAX_THREADS {
        return frame;
    }
    table.threads[tid].frame = frame as u64;
    // A sleeping thread is not runnable; the scheduler records the wake time
    // and puts it back on the run queue from the timer path.
    table.threads[tid].state = ThreadState::Blocked;
    // Define the syscall result now: the scheduler resumes this frame when the
    // sleep timer fires, and the ABI says a completed sleep returns zero.
    ret(frame, 0);
    drop(table);
    sched::user_sleep(frame, duration_ns)
}

/// Handle the `send` syscall. May block the calling thread.
pub fn send_current(
    frame: *mut TrapFrame,
    target: u64,
    address: u64,
    length: u64,
) -> *mut TrapFrame {
    let mut table = TABLE.lock();
    sync_current(&mut table);
    let tid = table.current;
    if tid >= MAX_THREADS {
        return frame;
    }
    let pid = table.threads[tid].pid;
    if target == 0 || target as usize >= MAX_PROCESSES {
        return ret(frame, ESRCH);
    }
    let target = target as usize;
    if table.processes[target].state != ProcessState::Live {
        return ret(frame, ESRCH);
    }
    if target == pid {
        return ret(frame, EINVAL);
    }
    if length == 0 || length > MAX_MESSAGE as u64 {
        return ret(frame, EMSGSIZE);
    }
    let mut data = [0u8; MAX_MESSAGE];
    if let Err(error) = copy_from_process(&table, pid, address, &mut data[..length as usize]) {
        return ret(frame, error);
    }

    let (waiting, wanted, buffer, capacity, has_pending) = {
        let ipc = &table.processes[target].ipc;
        (
            ipc.recv_waiting,
            ipc.recv_from,
            ipc.recv_buffer,
            ipc.recv_capacity,
            ipc.pending.is_some(),
        )
    };

    if waiting {
        if wanted != 0 && wanted != pid as u64 {
            return ret(frame, EAGAIN);
        }
        if capacity < length {
            table.processes[target].ipc.recv_waiting = false;
            if let Some(receiver) = blocked_thread_of(&table, target) {
                wake_thread(&mut table, receiver, EMSGSIZE);
            }
            return ret(frame, EMSGSIZE);
        }
        if let Err(error) = copy_to_process(
            &table,
            target,
            buffer,
            &data[..length as usize],
        ) {
            table.processes[target].ipc.recv_waiting = false;
            if let Some(receiver) = blocked_thread_of(&table, target) {
                wake_thread(&mut table, receiver, error);
            }
            return ret(frame, error);
        }
        table.processes[target].ipc.recv_waiting = false;
        if let Some(receiver) = blocked_thread_of(&table, target) {
            wake_thread(&mut table, receiver, length as i64);
        }
        return ret(frame, length as i64);
    }

    if has_pending {
        return ret(frame, EAGAIN);
    }

    table.processes[target].ipc.pending = Some(Message {
        from_pid: pid as u64,
        length: length as usize,
        data,
        sender_thread: tid,
    });
    block_current(&mut table, frame)
}

/// Handle the `recv` syscall. May block the calling thread.
pub fn recv_current(
    frame: *mut TrapFrame,
    from: u64,
    address: u64,
    length: u64,
) -> *mut TrapFrame {
    let mut table = TABLE.lock();
    sync_current(&mut table);
    let tid = table.current;
    if tid >= MAX_THREADS {
        return frame;
    }
    let pid = table.threads[tid].pid;
    if length == 0 || length > MAX_MESSAGE as u64 {
        return ret(frame, EMSGSIZE);
    }
    if let Err(error) = validate_user_write(&table, pid, address, length as usize) {
        return ret(frame, error);
    }

    let pending = table.processes[pid].ipc.pending;
    if let Some(message) = pending {
        if from == 0 || message.from_pid == from {
            if length < message.length as u64 {
                return ret(frame, EMSGSIZE);
            }
            if let Err(error) =
                copy_to_process(&table, pid, address, &message.data[..message.length])
            {
                return ret(frame, error);
            }
            table.processes[pid].ipc.pending = None;
            wake_thread(&mut table, message.sender_thread, message.length as i64);
            return ret(frame, message.length as i64);
        }
    }

    let ipc = &mut table.processes[pid].ipc;
    ipc.recv_waiting = true;
    ipc.recv_from = from;
    ipc.recv_buffer = address;
    ipc.recv_capacity = length;
    block_current(&mut table, frame)
}

fn validate_user_write(table: &Table, pid: usize, address: u64, length: usize) -> Result<(), i64> {
    let end = address.checked_add(length as u64).ok_or(EFAULT)?;
    let space = table.processes[pid]
        .space
        .as_ref()
        .ok_or(ESRCH)?;
    let mut cursor = address;
    while cursor < end {
        let translation = space.translate(cursor).ok_or(EFAULT)?;
        if !translation.perm.user || !translation.perm.write {
            return Err(EFAULT);
        }
        let next = (cursor & !(PAGE_SIZE - 1)) + PAGE_SIZE;
        if next <= cursor {
            return Err(EFAULT);
        }
        cursor = next;
    }
    Ok(())
}

fn copy_from_process(
    table: &Table,
    pid: usize,
    address: u64,
    out: &mut [u8],
) -> Result<(), i64> {
    let end = address.checked_add(out.len() as u64).ok_or(EFAULT)?;
    let space = table.processes[pid].space.as_ref().ok_or(ESRCH)?;
    let mut copied = 0usize;
    while copied < out.len() {
        let virtual_address = address + copied as u64;
        let translation = space.translate(virtual_address).ok_or(EFAULT)?;
        if !translation.perm.user || !translation.perm.read {
            return Err(EFAULT);
        }
        let page_offset = (virtual_address & (PAGE_SIZE - 1)) as usize;
        let chunk = (PAGE_SIZE as usize - page_offset).min(out.len() - copied);
        if virtual_address + chunk as u64 > end {
            return Err(EFAULT);
        }
        // SAFETY: the translation covers this chunk in a live user page and the
        // destination is the caller's buffer of exactly `out.len()` bytes.
        unsafe {
            ptr::copy_nonoverlapping(
                phys_to_virt(translation.physical) as *const u8,
                out.as_mut_ptr().add(copied),
                chunk,
            );
        }
        copied += chunk;
    }
    Ok(())
}

fn copy_to_process(
    table: &Table,
    pid: usize,
    address: u64,
    data: &[u8],
) -> Result<(), i64> {
    let end = address.checked_add(data.len() as u64).ok_or(EFAULT)?;
    let space = table.processes[pid].space.as_ref().ok_or(ESRCH)?;
    let mut copied = 0usize;
    while copied < data.len() {
        let virtual_address = address + copied as u64;
        let translation = space.translate(virtual_address).ok_or(EFAULT)?;
        if !translation.perm.user || !translation.perm.write {
            return Err(EFAULT);
        }
        let page_offset = (virtual_address & (PAGE_SIZE - 1)) as usize;
        let chunk = (PAGE_SIZE as usize - page_offset).min(data.len() - copied);
        if virtual_address + chunk as u64 > end {
            return Err(EFAULT);
        }
        // SAFETY: see `copy_from_process`.
        unsafe {
            ptr::copy_nonoverlapping(
                data.as_ptr().add(copied),
                phys_to_virt(translation.physical) as *mut u8,
                chunk,
            );
        }
        copied += chunk;
    }
    Ok(())
}

/// Copy a message from the current user process for the `write` syscall.
pub fn copy_from_current_user(address: u64, out: &mut [u8]) -> Result<(), i64> {
    let mut table = TABLE.lock();
    sync_current(&mut table);
    let tid = table.current;
    if tid >= MAX_THREADS {
        return Err(ESRCH);
    }
    let pid = table.threads[tid].pid;
    copy_from_process(&table, pid, address, out)
}

/// Report every process and finish the milestone.
pub fn user_phase_complete() -> ! {
    report();
    #[cfg(feature = "shell")]
    shell::run();
    #[cfg(not(feature = "shell"))]
    finish()
}

/// Print the process summary and request power-off.
pub fn finish() -> ! {
    println!();
    println!("SKETCHOS 0.0.6 KERNEL OK");

    // Keep another timer tick from interrupting the final halt/power-off path.
    gic::disable_irq();
    if let Some(use_hvc) = smp::power_off() {
        println!("QEMU: requesting PSCI SYSTEM_OFF");
        // SAFETY: the FADT declared PSCI and the conduit used here.
        match unsafe { aarch64::psci_system_off(use_hvc) } {
            Ok(()) => println!("QEMU: PSCI SYSTEM_OFF returned unexpectedly"),
            Err(code) => println!("QEMU: PSCI SYSTEM_OFF rejected (error {})", code),
        }
    }
    println!("Halting: userspace phase complete");
    aarch64::halt()
}

/// Report a user-phase setup failure and power the machine off without the
/// success marker.
pub fn fail(reason: &str) -> ! {
    println!();
    println!("SKETCHOS 0.0.6 KERNEL FAILED: {}", reason);
    gic::disable_irq();
    if let Some(use_hvc) = smp::power_off() {
        println!("QEMU: requesting PSCI SYSTEM_OFF");
        // SAFETY: the FADT declared PSCI and the conduit used here.
        match unsafe { aarch64::psci_system_off(use_hvc) } {
            Ok(()) => println!("QEMU: PSCI SYSTEM_OFF returned unexpectedly"),
            Err(code) => println!("QEMU: PSCI SYSTEM_OFF rejected (error {})", code),
        }
    }
    println!("Halting: userspace setup failed");
    aarch64::halt()
}

fn report() {
    let table = TABLE.lock();
    println!();
    println!(
        "Userspace: summary {} processes, {} threads",
        table.live_processes + count_zombies(&table),
        table.live_threads
    );
    for pid in 1..MAX_PROCESSES {
        let process = &table.processes[pid];
        if process.state == ProcessState::Zombie {
            println!(
                "  pid {}: {}, status {}, {} user pages",
                process.pid,
                if process.faulted {
                    "terminated by fault"
                } else {
                    "exited"
                },
                process.exit_status,
                process.page_count
            );
        }
    }
    drop(table);
    sched::report_latency();
}

fn count_zombies(table: &Table) -> usize {
    table
        .processes
        .iter()
        .filter(|process| process.state == ProcessState::Zombie)
        .count()
}

fn round_up(value: u64, alignment: u64) -> u64 {
    if value == 0 {
        0
    } else {
        (value + alignment - 1) & !(alignment - 1)
    }
}
