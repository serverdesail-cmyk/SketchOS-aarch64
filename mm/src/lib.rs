//! SketchOS memory-management primitives.
//!
//! This crate is `no_std` so the kernel can link it directly, and it has no
//! dependency on the boot ABI. The kernel translates its UEFI descriptors into
//! [`physical::MemoryRange`] values; host tests use the same API with synthetic
//! maps.
//!
//! Two allocators live here:
//!
//! * [`physical::PhysicalAllocator`] - a bitmap allocator for 4 KiB physical
//!   pages. It keeps separate "managed" and "allocated" bitmaps, which lets a
//!   free distinguish an address outside the managed map (invalid free) from a
//!   managed page that is already free (double free).
//! * [`heap::Heap`] - an address-ordered first-fit free-list heap with
//!   coalescing, arbitrary power-of-two alignment and fragmentation statistics.
//!
//! [`pagetable`] holds the shared AArch64 stage-1 table walker used by the
//! loader, the kernel and host tests.
//!
//! Both allocators are deliberately small and explicit. They do not allocate
//! metadata from themselves and they never silently corrupt state on failure.

#![cfg_attr(not(test), no_std)]

pub mod heap;
pub mod pagetable;
pub mod physical;
