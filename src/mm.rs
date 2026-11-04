//! Physical page allocator and kernel heap.
//!
//! Pages: one bit per 4 KiB page from address 0 up to the end of the highest
//! conventional region in the UEFI memory map (1 = used). The bitmap itself
//! lives in the first conventional region large enough to hold it.
//!
//! Heap: an address-ordered free list of 16-byte-granular blocks, first fit,
//! coalescing neighbours on free. It grows by taking pages from the page
//! allocator; pages are never returned.
//!
//! Single core: both are shared with preemptible tasks by masking IRQs.

use crate::global::Global;
use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr;
use uefi::boot::MemoryType;
use uefi::mem::memory_map::MemoryMap;

pub const PAGE_SIZE: usize = 4096;

pub struct PageAllocator {
    bitmap: *mut u8,
    pages: usize,
    free: usize,
    usable: usize,
    hint: usize,
}

// SAFETY: the bitmap belongs to the allocator alone; nothing else points into it.
unsafe impl Send for PageAllocator {}

impl PageAllocator {
    fn is_used(&self, page: usize) -> bool {
        // SAFETY: callers pass `page < self.pages`, and the bitmap holds `self.pages` bits.
        unsafe { *self.bitmap.add(page / 8) & (1 << (page % 8)) != 0 }
    }

    fn set(&mut self, page: usize, used: bool) {
        // SAFETY: as in `is_used`; `&mut self` makes this the only reference.
        let byte = unsafe { &mut *self.bitmap.add(page / 8) };
        if used {
            *byte |= 1 << (page % 8);
        } else {
            *byte &= !(1 << (page % 8));
        }
    }

    /// Allocates `count` physically contiguous pages, returning the address.
    pub fn alloc(&mut self, count: usize) -> Option<usize> {
        let start = self
            .find_run(self.hint, self.pages, count)
            .or_else(|| self.find_run(1, self.hint, count))?;
        for p in start..start + count {
            self.set(p, true);
        }
        self.free -= count;
        self.hint = start + count;
        Some(start * PAGE_SIZE)
    }

    fn find_run(&self, from: usize, to: usize, count: usize) -> Option<usize> {
        let mut run = 0;
        for p in from..to {
            if self.is_used(p) {
                run = 0;
            } else {
                run += 1;
                if run == count {
                    return Some(p + 1 - count);
                }
            }
        }
        None
    }

    /// Gives back `count` pages at `addr`.
    ///
    /// # Safety
    ///
    /// The pages must have come from `alloc`, and nothing may use them afterwards.
    ///
    /// # Panics
    ///
    /// If `addr` isn't page aligned or a page is already free.
    pub unsafe fn free(&mut self, addr: usize, count: usize) {
        assert!(addr.is_multiple_of(PAGE_SIZE), "unaligned page free {:#x}", addr);
        let first = addr / PAGE_SIZE;
        for p in first..first + count {
            assert!(self.is_used(p), "double free of page {:#x}", p * PAGE_SIZE);
            self.set(p, false);
        }
        self.free += count;
        self.hint = self.hint.min(first);
    }

    pub fn free_pages(&self) -> usize {
        self.free
    }

    pub fn usable_pages(&self) -> usize {
        self.usable
    }

    pub fn bitmap_addr(&self) -> usize {
        self.bitmap as usize
    }
}

static PAGES: Global<PageAllocator> = Global::new("page allocator");

/// Runs `f` with the page allocator. Panics before `init`.
pub fn pages<R>(f: impl FnOnce(&mut PageAllocator) -> R) -> R {
    PAGES.with(f)
}

pub fn init(mmap: &impl MemoryMap) {
    let conventional = || mmap.entries().filter(|d| d.ty == MemoryType::CONVENTIONAL);

    let end = conventional()
        .map(|d| d.phys_start as usize + d.page_count as usize * PAGE_SIZE)
        .max()
        .expect("no conventional memory");
    let pages = end / PAGE_SIZE;
    let bitmap_pages = pages.div_ceil(8).div_ceil(PAGE_SIZE);
    let bitmap = conventional()
        .find(|d| d.phys_start != 0 && d.page_count as usize >= bitmap_pages)
        .expect("no region large enough for the page bitmap")
        .phys_start as usize;

    // Everything starts out used; conventional regions are then released.
    // SAFETY: conventional memory that nothing uses yet, identity mapped, and large enough.
    unsafe { ptr::write_bytes(bitmap as *mut u8, 0xff, bitmap_pages * PAGE_SIZE) };
    let mut pa = PageAllocator { bitmap: bitmap as *mut u8, pages, free: 0, usable: 0, hint: 1 };
    for d in conventional() {
        let first = d.phys_start as usize / PAGE_SIZE;
        for p in first..first + d.page_count as usize {
            pa.set(p, false);
        }
        pa.free += d.page_count as usize;
    }
    // Page 0 stays reserved so that a null pointer is never a valid allocation.
    for p in (0..1).chain(bitmap / PAGE_SIZE..bitmap / PAGE_SIZE + bitmap_pages) {
        if !pa.is_used(p) {
            pa.set(p, true);
            pa.free -= 1;
        }
    }
    pa.usable = pa.free;
    PAGES.set(pa);
}

// ---------------------------------------------------------------------------
// Heap

const ALIGN: usize = 16;
/// Minimum heap growth, in pages (1 MiB).
const GROW_PAGES: usize = 256;

struct Block {
    size: usize,
    next: *mut Block,
}

const _: () = assert!(core::mem::size_of::<Block>() <= ALIGN);

pub struct Heap {
    head: UnsafeCell<*mut Block>,
    grown: UnsafeCell<usize>,
}

// SAFETY: one CPU, and `head` and `grown` are only touched with IRQs masked (in `alloc`,
// `dealloc`, and `grown_bytes`), so accesses never overlap.
unsafe impl Sync for Heap {}

fn round_up(x: usize, align: usize) -> usize {
    (x + align - 1) & !(align - 1)
}

fn block_size(layout: &Layout) -> usize {
    round_up(layout.size().max(ALIGN), ALIGN)
}

impl Heap {
    const fn new() -> Self {
        Self { head: UnsafeCell::new(ptr::null_mut()), grown: UnsafeCell::new(0) }
    }

    /// Bytes the heap has taken from the page allocator so far.
    pub fn grown_bytes(&self) -> usize {
        // SAFETY: see `Sync` above.
        crate::arch::without_irqs(|| unsafe { *self.grown.get() })
    }

    /// Inserts a free region in address order, merging with its neighbours.
    ///
    /// # Safety
    ///
    /// IRQs masked. `addr..addr + size` is unused memory, both multiples of `ALIGN`.
    unsafe fn insert(&self, addr: usize, size: usize) {
        // SAFETY: the list holds only free blocks the heap owns; the new one is the caller's.
        unsafe {
            let head = &mut *self.head.get();
            let mut prev: *mut Block = ptr::null_mut();
            let mut cur = *head;
            while !cur.is_null() && (cur as usize) < addr {
                prev = cur;
                cur = (*cur).next;
            }

            let block = addr as *mut Block;
            block.write(Block { size, next: cur });
            if !cur.is_null() && addr + size == cur as usize {
                (*block).size += (*cur).size;
                (*block).next = (*cur).next;
            }

            if prev.is_null() {
                *head = block;
            } else if prev as usize + (*prev).size == addr {
                (*prev).size += (*block).size;
                (*prev).next = (*block).next;
            } else {
                (*prev).next = block;
            }
        }
    }

    /// First fit. All addresses and sizes are multiples of `ALIGN`, so any
    /// leftover in front of or behind the allocation is itself a valid block.
    ///
    /// # Safety
    ///
    /// IRQs masked; `size` and `align` are multiples of `ALIGN`.
    unsafe fn take(&self, size: usize, align: usize) -> *mut u8 {
        // SAFETY: walks blocks the heap owns; the leftovers handed to `insert` are free and
        // aligned (see above).
        unsafe {
            let head = &mut *self.head.get();
            let mut prev: *mut Block = ptr::null_mut();
            let mut cur = *head;
            while !cur.is_null() {
                let start = cur as usize;
                let end = start + (*cur).size;
                let addr = round_up(start, align);
                if addr + size <= end {
                    let next = (*cur).next;
                    if prev.is_null() {
                        *head = next;
                    } else {
                        (*prev).next = next;
                    }
                    if addr > start {
                        self.insert(start, addr - start);
                    }
                    if addr + size < end {
                        self.insert(addr + size, end - addr - size);
                    }
                    return addr as *mut u8;
                }
                prev = cur;
                cur = (*cur).next;
            }
            ptr::null_mut()
        }
    }

    /// Takes pages for at least one `size` block at `align`.
    ///
    /// # Safety
    ///
    /// IRQs masked.
    unsafe fn grow(&self, size: usize, align: usize) -> bool {
        let count = GROW_PAGES.max((size + align).div_ceil(PAGE_SIZE));
        // `try_with`: an allocation inside `mm::pages` fails instead of re-entering it.
        let Some(addr) = PAGES.try_with(|pa| pa.alloc(count)).flatten() else {
            return false;
        };
        // SAFETY: the pages are fresh and page aligned; IRQs are masked (caller).
        unsafe {
            *self.grown.get() += count * PAGE_SIZE;
            self.insert(addr, count * PAGE_SIZE);
        }
        true
    }
}

// SAFETY: blocks handed out are taken off the free list, so they never overlap, and are
// aligned to at least `layout.align()`.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = block_size(&layout);
        let align = layout.align().max(ALIGN);
        // SAFETY: IRQs are masked, and sizes are rounded to `ALIGN`.
        crate::arch::without_irqs(|| unsafe {
            let p = self.take(size, align);
            if !p.is_null() || !self.grow(size, align) {
                return p;
            }
            self.take(size, align)
        })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `alloc` with this layout, so it is a whole free block now.
        crate::arch::without_irqs(|| unsafe { self.insert(ptr as usize, block_size(&layout)) });
    }
}

#[global_allocator]
pub static HEAP: Heap = Heap::new();
