use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::{PersistentId, marker};

#[cfg(target_arch = "wasm32")]
const WASM_PAGE_SIZE: u32 = 64 * 1024;
const PERSISTENT_HEAP_CURSOR_OFFSET: u32 = 12 * 1024;
const PERSISTENT_HEAP_BASE: u32 = 128 * 1024;

static TRANSACTION_DEPTH: AtomicU32 = AtomicU32::new(0);

#[cfg(target_arch = "wasm32")]
static VOLATILE_CURSOR: AtomicU32 = AtomicU32::new(0);

pub struct TxAllocator;

pub struct TransactionAllocGuard;

impl TxAllocator {
    pub const fn new() -> Self {
        Self
    }
}

impl Default for TxAllocator {
    fn default() -> Self {
        Self::new()
    }
}

pub fn enter_transaction() -> TransactionAllocGuard {
    TRANSACTION_DEPTH.fetch_add(1, Ordering::Relaxed);
    TransactionAllocGuard
}

pub fn in_transaction() -> bool {
    TRANSACTION_DEPTH.load(Ordering::Relaxed) != 0
}

impl Drop for TransactionAllocGuard {
    fn drop(&mut self) {
        TRANSACTION_DEPTH.fetch_sub(1, Ordering::Relaxed);
    }
}

unsafe impl GlobalAlloc for TxAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if in_transaction() {
            unsafe { persistent_alloc(layout) }
        } else {
            unsafe { volatile_alloc(layout) }
        }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if in_transaction() {
            unsafe { persistent_alloc_zeroed(layout) }
        } else {
            unsafe { volatile_alloc_zeroed(layout) }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if in_transaction() {
            unsafe { persistent_dealloc(ptr, layout) };
        } else {
            unsafe { volatile_dealloc(ptr, layout) };
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if in_transaction() {
            unsafe { persistent_realloc(ptr, layout, new_size) }
        } else {
            unsafe { volatile_realloc(ptr, layout, new_size) }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
unsafe fn volatile_alloc(layout: Layout) -> *mut u8 {
    unsafe { std::alloc::System.alloc(layout) }
}

#[cfg(not(target_arch = "wasm32"))]
unsafe fn volatile_alloc_zeroed(layout: Layout) -> *mut u8 {
    unsafe { std::alloc::System.alloc_zeroed(layout) }
}

#[cfg(not(target_arch = "wasm32"))]
unsafe fn volatile_dealloc(ptr: *mut u8, layout: Layout) {
    unsafe { std::alloc::System.dealloc(ptr, layout) };
}

#[cfg(not(target_arch = "wasm32"))]
unsafe fn volatile_realloc(ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    unsafe { std::alloc::System.realloc(ptr, layout, new_size) }
}

#[cfg(target_arch = "wasm32")]
unsafe fn volatile_alloc(layout: Layout) -> *mut u8 {
    let size = layout.size().max(1);
    let align = layout.align().max(1);

    loop {
        let current = volatile_cursor();
        let aligned = align_up_usize(current, align);
        let Some(end) = aligned.checked_add(size) else {
            return ptr::null_mut();
        };
        if !ensure_wasm_memory(end) {
            return ptr::null_mut();
        }

        if VOLATILE_CURSOR
            .compare_exchange(
                current as u32,
                end as u32,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            return aligned as *mut u8;
        }
    }
}

#[cfg(target_arch = "wasm32")]
unsafe fn volatile_dealloc(_ptr: *mut u8, _layout: Layout) {}

#[cfg(target_arch = "wasm32")]
unsafe fn volatile_alloc_zeroed(layout: Layout) -> *mut u8 {
    let ptr = unsafe { volatile_alloc(layout) };
    if !ptr.is_null() {
        unsafe {
            ptr::write_bytes(ptr, 0, layout.size());
        }
    }
    ptr
}

#[cfg(target_arch = "wasm32")]
unsafe fn volatile_realloc(ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    let new_ptr = unsafe {
        volatile_alloc(Layout::from_size_align_unchecked(
            new_size,
            layout.align().max(1),
        ))
    };
    if !new_ptr.is_null() {
        unsafe {
            ptr::copy_nonoverlapping(ptr, new_ptr, layout.size().min(new_size));
        }
    }
    new_ptr
}

#[cfg(target_arch = "wasm32")]
fn volatile_cursor() -> usize {
    let current = VOLATILE_CURSOR.load(Ordering::Relaxed);
    if current != 0 {
        return current as usize;
    }

    let heap_base = core::ptr::addr_of!(__heap_base) as usize;
    let heap_base = align_up_usize(heap_base, 16);
    let _ =
        VOLATILE_CURSOR.compare_exchange(0, heap_base as u32, Ordering::Relaxed, Ordering::Relaxed);
    VOLATILE_CURSOR.load(Ordering::Relaxed) as usize
}

#[cfg(target_arch = "wasm32")]
fn ensure_wasm_memory(required_end: usize) -> bool {
    let current_pages = core::arch::wasm32::memory_size(0);
    let current_bytes = current_pages.saturating_mul(WASM_PAGE_SIZE as usize);
    if required_end <= current_bytes {
        return true;
    }

    let needed_bytes = required_end - current_bytes;
    let needed_pages = needed_bytes.div_ceil(WASM_PAGE_SIZE as usize);
    core::arch::wasm32::memory_grow(0, needed_pages) != usize::MAX
}

#[cfg(target_arch = "wasm32")]
unsafe extern "C" {
    static __heap_base: u8;
}

#[inline(never)]
unsafe fn persistent_alloc(layout: Layout) -> *mut u8 {
    marker::mark_transaction_func();

    let size = layout.size().max(1);
    let align = layout.align().max(1);
    let cursor_ptr = unsafe {
        marker::persistent_addr_mut(PersistentId::tmemory_offset(PERSISTENT_HEAP_CURSOR_OFFSET))
    } as *mut u32;
    let cursor = unsafe { cursor_ptr.read() };
    let cursor = if cursor < PERSISTENT_HEAP_BASE {
        PERSISTENT_HEAP_BASE
    } else {
        cursor
    };

    let Some(aligned) = align_up_u32(cursor, align) else {
        return ptr::null_mut();
    };
    let Some(end) = aligned.checked_add(u32::try_from(size).unwrap_or(u32::MAX)) else {
        return ptr::null_mut();
    };

    unsafe {
        cursor_ptr.write(end);
        marker::persistent_addr_mut(PersistentId::tmemory_offset(aligned))
    }
}

#[inline(never)]
unsafe fn persistent_alloc_zeroed(layout: Layout) -> *mut u8 {
    marker::mark_transaction_func();

    let ptr = unsafe { persistent_alloc(layout) };
    if !ptr.is_null() {
        for offset in 0..layout.size() {
            unsafe {
                ptr.add(offset).write(0);
            }
        }
    }
    ptr
}

#[inline(never)]
unsafe fn persistent_dealloc(ptr: *mut u8, _layout: Layout) {
    marker::mark_transaction_func();
    marker::mark_persistent_arg(0);
    let _ = ptr;
}

#[inline(never)]
unsafe fn persistent_realloc(ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    marker::mark_transaction_func();
    marker::mark_persistent_arg(0);

    let new_ptr = unsafe {
        persistent_alloc(Layout::from_size_align_unchecked(
            new_size,
            layout.align().max(1),
        ))
    };
    if !new_ptr.is_null() {
        for offset in 0..layout.size().min(new_size) {
            unsafe {
                let byte = ptr.add(offset).read();
                new_ptr.add(offset).write(byte);
            }
        }
    }
    new_ptr
}

#[cfg(target_arch = "wasm32")]
fn align_up_usize(value: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (value + align - 1) & !(align - 1)
}

fn align_up_u32(value: u32, align: usize) -> Option<u32> {
    let align = u32::try_from(align).ok()?;
    debug_assert!(align.is_power_of_two());
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_guard_tracks_nested_depth() {
        assert!(!in_transaction());
        let outer = enter_transaction();
        assert!(in_transaction());
        {
            let _inner = enter_transaction();
            assert!(in_transaction());
        }
        assert!(in_transaction());
        drop(outer);
        assert!(!in_transaction());
    }
}
