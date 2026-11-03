//! The user program's heap: a first-fit free list over a static arena.
//!
//! The same scheme as the kernel heap (address-ordered free blocks, 16-byte
//! granules, neighbours merged on free), but it never grows: the arena is
//! in .bss, so every copy of the program has its own in its own address
//! space. A program runs as a single task, so no locking is needed.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr;

const SIZE: usize = 256 * 1024;
const ALIGN: usize = 16;

struct Block {
    size: usize,
    next: *mut Block,
}

#[repr(C, align(16))]
struct Heap {
    arena: UnsafeCell<[u8; SIZE]>,
    head: UnsafeCell<*mut Block>,
    /// Whether the arena has been put on the free list yet.
    ready: UnsafeCell<bool>,
}

unsafe impl Sync for Heap {}

#[global_allocator]
static HEAP: Heap = Heap {
    arena: UnsafeCell::new([0; SIZE]),
    head: UnsafeCell::new(ptr::null_mut()),
    ready: UnsafeCell::new(false),
};

fn block_size(layout: &Layout) -> usize {
    layout.size().max(ALIGN).next_multiple_of(ALIGN)
}

impl Heap {
    /// Inserts a free region in address order, merging with its neighbours.
    unsafe fn insert(&self, addr: usize, size: usize) {
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
}

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let (size, align) = (block_size(&layout), layout.align().max(ALIGN));
        unsafe {
            if !core::mem::replace(&mut *self.ready.get(), true) {
                self.insert(self.arena.get() as usize, SIZE);
            }
            let head = &mut *self.head.get();
            let mut prev: *mut Block = ptr::null_mut();
            let mut cur = *head;
            while !cur.is_null() {
                let (start, end) = (cur as usize, cur as usize + (*cur).size);
                let addr = start.next_multiple_of(align);
                if addr + size <= end {
                    let next = (*cur).next;
                    if prev.is_null() {
                        *head = next;
                    } else {
                        (*prev).next = next;
                    }
                    // Leftovers are multiples of 16 and become free blocks.
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

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.insert(ptr as usize, block_size(&layout)) };
    }
}

/// Bytes on the free list.
pub fn free_bytes() -> usize {
    let mut total = 0;
    let mut cur = unsafe { *HEAP.head.get() };
    while !cur.is_null() {
        unsafe {
            total += (*cur).size;
            cur = (*cur).next;
        }
    }
    if unsafe { *HEAP.ready.get() } { total } else { SIZE }
}
