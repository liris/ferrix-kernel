#![no_std]
#![no_main]

mod arch;
mod console;

use console::{ACCENT, DIM, ERR, FG, Format, Framebuffer, OK};
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
    rsdp: Option<usize>,
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
    println!("[boot] framebuffer {}x{} stride={}", info.fb.width, info.fb.height, info.fb.stride);

    // SAFETY: nothing borrowed from boot services is used after this point;
    // the framebuffer is plain memory that stays mapped.
    let mmap = unsafe { boot::exit_boot_services(None) };
    ok("ExitBootServices: now running on our own");

    kernel_main(&info, &mmap)
}

fn collect_boot_info() -> Result<BootInfo, Status> {
    let handle = boot::get_handle_for_protocol::<GraphicsOutput>().map_err(|e| e.status())?;
    let mut gop = boot::open_protocol_exclusive::<GraphicsOutput>(handle).map_err(|e| e.status())?;
    let mode = gop.current_mode_info();
    let format = match mode.pixel_format() {
        PixelFormat::Rgb => Format::Rgb,
        PixelFormat::Bgr => Format::Bgr,
        _ => return Err(Status::UNSUPPORTED),
    };
    let (width, height) = mode.resolution();
    let fb = Framebuffer {
        base: gop.frame_buffer().as_mut_ptr().cast(),
        width,
        height,
        stride: mode.stride(),
        format,
    };

    let rsdp = system::with_config_table(|entries| {
        entries
            .iter()
            .find(|e| e.guid == ConfigTableEntry::ACPI2_GUID)
            .map(|e| e.address as usize)
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
        Some(addr) => println!("[acpi] RSDP at {:#x}", addr),
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
    println!();

    // Poll the generic timer and redraw an uptime line once per second.
    let line = console::with(|c| c.row()).unwrap_or(0);
    let start = arch::timer_count();
    let mut last = u64::MAX;
    loop {
        let secs = (arch::timer_count() - start) / freq.max(1);
        if secs != last {
            last = secs;
            console::with(|c| {
                c.goto_line(line);
                c.set_color(DIM);
            });
            print!("uptime {:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
            console::with(|c| c.set_color(FG));
        }
        core::hint::spin_loop();
    }
}

fn banner() {
    console::with(|c| c.set_color(ACCENT));
    println!("ferrix-kernel {} (aarch64)", VERSION);
    console::with(|c| c.set_color(DIM));
    println!("a tiny Rust kernel for VMware Fusion on Apple Silicon");
    console::with(|c| c.set_color(FG));
    println!();
}

fn ok(msg: &str) {
    console::with(|c| c.set_color(OK));
    print!("[ ok ] ");
    console::with(|c| c.set_color(FG));
    println!("{}", msg);
}

/// The UEFI target links like Windows and expects libc's `wcslen` (UTF-16).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcslen(s: *const u16) -> usize {
    let mut n = 0;
    while unsafe { *s.add(n) } != 0 {
        n += 1;
    }
    n
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    console::with(|c| c.set_color(ERR));
    println!("\n*** KERNEL PANIC ***");
    println!("{}", info);
    arch::halt()
}
