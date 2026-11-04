#![no_std]
#![no_main]

extern crate alloc;

mod arch;
mod demo;
mod dev;
mod global;
mod init;
mod mm;
mod mmio;
mod proc;

use core::arch::asm;
use dev::console::{self, ACCENT, DIM, ERR, FG, Format, Framebuffer, OK};
use dev::{acpi, gic, irq};
use uefi::boot::{self, MemoryType};
use uefi::mem::memory_map::MemoryMap;
use uefi::prelude::*;
use uefi::proto::console::gop::{GraphicsOutput, PixelFormat};
use uefi::system;
use uefi::table::cfg::ConfigTableEntry;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What the kernel learns from the firmware before leaving boot services.
struct BootInfo {
    fb: Framebuffer,
    rsdp: Option<acpi::Rsdp>,
}

#[entry]
fn efi_main() -> Status {
    let info = match collect_boot_info() {
        Ok(info) => info,
        Err(status) => return status,
    };

    console::init(info.fb);
    console::with(|c| c.clear());
    banner();
    println!(
        "[boot] framebuffer {}x{} stride={}",
        info.fb.width(),
        info.fb.height(),
        info.fb.stride()
    );

    // SAFETY: nothing borrowed from boot services is used after this point;
    // the framebuffer is plain memory that stays mapped.
    let mmap = unsafe { boot::exit_boot_services(None) };
    ok("ExitBootServices: now running on our own");

    kernel_main(&info, &mmap)
}

fn collect_boot_info() -> Result<BootInfo, Status> {
    let handle = boot::get_handle_for_protocol::<GraphicsOutput>().map_err(|e| e.status())?;
    let mut gop =
        boot::open_protocol_exclusive::<GraphicsOutput>(handle).map_err(|e| e.status())?;
    let mode = gop.current_mode_info();
    let format = match mode.pixel_format() {
        PixelFormat::Rgb => Format::Rgb,
        PixelFormat::Bgr => Format::Bgr,
        _ => return Err(Status::UNSUPPORTED),
    };
    let (width, height) = mode.resolution();
    let base = gop.frame_buffer().as_mut_ptr().cast();
    // SAFETY: GOP's framebuffer holds `stride * height` 32-bit pixels for the current mode,
    // and after ExitBootServices nothing but the console touches it.
    let fb = unsafe { Framebuffer::new(base, width, height, mode.stride(), format) };

    let rsdp = system::with_config_table(|entries| {
        entries
            .iter()
            .find(|e| e.guid == ConfigTableEntry::ACPI2_GUID)
            // SAFETY: the firmware published this RSDP. ACPI tables aren't conventional
            // memory, so the page allocator never hands them out, and the identity map
            // keeps them mapped.
            .map(|e| unsafe { acpi::Rsdp::new(e.address as usize) })
    });

    Ok(BootInfo { fb, rsdp })
}

fn kernel_main(info: &BootInfo, mmap: &impl MemoryMap) -> ! {
    let midr = arch::midr();
    println!(
        "[cpu ] EL{}  MIDR={:#010x} ({})  part={:#05x}",
        arch::current_el(),
        midr,
        arch::implementer_name(midr),
        (midr >> 4) & 0xfff
    );
    let freq = arch::timer_freq();
    println!("[time] generic timer {} Hz", freq);
    match info.rsdp {
        Some(rsdp) => println!("[acpi] RSDP at {:#x}", rsdp.addr()),
        None => println!("[acpi] no ACPI 2.0 table"),
    }

    let mut usable_pages = 0u64;
    let mut total_pages = 0u64;
    for d in mmap.entries() {
        total_pages += d.page_count;
        if d.ty == MemoryType::CONVENTIONAL {
            usable_pages += d.page_count;
        }
    }
    println!(
        "[mem ] {} regions, {} MiB total, {} MiB usable",
        mmap.len(),
        total_pages * 4 / 1024,
        usable_pages * 4 / 1024
    );
    ok("M1: kernel is self-hosted");

    init::exceptions();
    init::memory(mmap);
    let gic = init::find_gic(info.rsdp);
    ok("M2: exceptions and memory");
    init::page_tables(&info.fb, mmap, gic.as_ref());
    demo::nand::run();

    if cfg!(feature = "fault-test") {
        println!("[test] dereferencing a null pointer...");
        // SAFETY: page 0 is never mapped, so the load raises a data abort, which panics.
        // It's an `asm!` load rather than a Rust one so no Rust-level UB is involved.
        unsafe { asm!("ldr {0}, [{0}]", inout(reg) 0u64 => _, options(nostack, readonly)) };
    }

    // SAFETY: `gic` came from `acpi::find_gic`, and `init_mmu` mapped its ranges.
    match gic.as_ref().map(|g| unsafe { gic::init(g) }) {
        Some(Ok(rd)) => {
            println!("[gic ] GICv3 up, redistributor at {:#x}", rd);
            irq::start_timer(freq);
            ok(format_args!("M3: virtual timer IRQ at {} Hz", irq::TICK_HZ));
            demo::tasks::start();
            let keyboard = init::usb(info.rsdp);
            let builder = init::user_programs();
            demo::screen::interrupt_loop(keyboard, builder)
        }
        Some(Err(e)) => println!("[gic ] {}", e),
        None => println!("[gic ] not found"),
    }
    println!("[gic ] falling back to polling the timer");
    println!();
    demo::screen::polling_loop(freq)
}

fn banner() {
    console::with(|c| c.set_color(ACCENT));
    println!("ferrix-kernel {} (aarch64)", VERSION);
    console::with(|c| c.set_color(DIM));
    println!("a tiny Rust kernel for VMware Fusion on Apple Silicon");
    console::with(|c| c.set_color(FG));
    println!();
}

fn ok(msg: impl core::fmt::Display) {
    console::with(|c| c.set_color(OK));
    print!("[ ok ] ");
    console::with(|c| c.set_color(FG));
    println!("{}", msg);
}

/// The UEFI target links like Windows and expects libc's `wcslen` (UTF-16).
///
/// # Safety
///
/// `s` must point to a NUL-terminated UTF-16 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcslen(s: *const u16) -> usize {
    let mut n = 0;
    // SAFETY: every unit up to the NUL is readable (caller).
    while unsafe { *s.add(n) } != 0 {
        n += 1;
    }
    n
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // Keep other tasks from running (and drawing) over the report.
    arch::disable_irqs();
    // SAFETY: IRQs are now off for good, and nothing returns from this handler.
    unsafe { console::reclaim() };
    console::with(|c| c.set_color(ERR));
    println!("\n*** KERNEL PANIC ***");
    println!("{}", info);
    arch::halt()
}
