#![no_std]
#![no_main]

extern crate alloc;

mod acpi;
mod arch;
mod console;
mod exception;
mod gic;
mod irq;
mod mm;
mod mmu;
mod nand;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use nand::{Circuit, Wire};
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

    init_exceptions();
    init_memory(mmap);
    let gic = report_gic(info.rsdp);
    ok("M2: exceptions and memory");
    init_mmu(&info.fb, mmap, gic.as_ref());
    nand_demo();

    if cfg!(feature = "fault-test") {
        println!("[test] dereferencing a null pointer...");
        let null = core::hint::black_box(core::ptr::null::<u64>());
        let _ = unsafe { null.read_volatile() };
    }

    match gic.as_ref().map(gic::init) {
        Some(Ok(rd)) => {
            println!("[gic ] GICv3 up, redistributor at {:#x}", rd);
            irq::start_timer(freq);
            ok(format_args!("M3: virtual timer IRQ at {} Hz", irq::TICK_HZ));
            interrupt_loop()
        }
        Some(Err(e)) => println!("[gic ] {}", e),
        None => println!("[gic ] not found"),
    }
    println!("[gic ] falling back to polling the timer");
    println!();
    polling_loop(freq)
}

/// Redraws `line` in the dim colour.
fn status_line(line: usize, args: core::fmt::Arguments) {
    console::with(|c| {
        c.goto_line(line);
        c.set_color(DIM);
    });
    print!("{}", args);
    console::with(|c| c.set_color(FG));
}

fn uptime(line: usize, secs: u64) {
    status_line(line, format_args!("uptime {:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60));
}

/// Without interrupts: spin on the generic timer, redraw once per second.
fn polling_loop(freq: u64) -> ! {
    let line = console::with(|c| c.row()).unwrap_or(0);
    let start = arch::timer_count();
    let mut last = u64::MAX;
    loop {
        let secs = (arch::timer_count() - start) / freq.max(1);
        if secs != last {
            last = secs;
            uptime(line, secs);
        }
        core::hint::spin_loop();
    }
}

/// Timer ticks per half clock period of the NAND counter (0.5 s at 10 Hz).
const HALF_PERIOD_TICKS: u64 = 5;

/// With interrupts: sleep in `wfi`; on each tick redraw the uptime and drive
/// the NAND counter's clock from the tick count.
fn interrupt_loop() -> ! {
    let mut c = Circuit::new();
    let (clk, one) = (c.input(), c.input());
    c.set(one, true);
    let q = c.counter::<4>(clk, one);
    c.eval().expect("counter did not settle");
    println!("[ctr ] 4-bit counter, {} NAND, timer-clocked", c.gate_count());
    // Reserve a blank spacer, then an uptime row and a counter row. Rows are
    // taken relative to the cursor, which stays valid if the screen scrolls.
    println!();
    println!();
    let line = console::with(|c| c.row()).unwrap_or(1) - 1;
    let mut seen = u64::MAX;
    let mut level = false;
    let mut edges = 0u64;
    let mut value = c.get_bits(&q);
    loop {
        let t = irq::ticks();
        if t != seen {
            seen = t;
            uptime(line, t / irq::TICK_HZ);

            let next_level = (t / HALF_PERIOD_TICKS) % 2 == 1;
            if next_level != level {
                level = next_level;
                c.set(clk, level);
                c.eval().expect("counter did not settle");
                let now = c.get_bits(&q);
                if level {
                    edges += 1;
                    if now != (value + 1) % 16 {
                        panic!("counter went {} -> {} on a rising edge", value, now);
                    }
                } else if now != value {
                    panic!("counter changed {} -> {} on a falling edge", value, now);
                }
                value = now;
            }
            status_line(
                line + 1,
                format_args!("clk={}  q={:04b} ({:2})  edges={}", level as u8, value, value, edges),
            );
        }
        arch::wait_for_interrupt();
    }
}

fn init_exceptions() {
    if arch::current_el() != 1 {
        panic!("expected EL1, running at EL{}", arch::current_el());
    }
    let vbar = exception::init();
    ok(format_args!("vectors at VBAR_EL1={:#x}", vbar));

    // Round-trip through both handled exception types.
    unsafe { core::arch::asm!("brk #0x42", clobber_abi("C")) };
    let ret: u64;
    unsafe { core::arch::asm!("svc #0x7", inout("x0") 41u64 => ret, clobber_abi("C")) };
    if ret != 42 {
        panic!("svc returned {} instead of 42", ret);
    }
    ok("brk/svc handled and resumed");
}

fn init_memory(mmap: &impl MemoryMap) {
    mm::init(mmap);
    let (free, usable, bitmap) = mm::pages(|pa| (pa.free_pages(), pa.usable_pages(), pa.bitmap_addr()));
    println!(
        "[mm  ] {} MiB free in 4K pages, bitmap at {:#x}",
        free * mm::PAGE_SIZE >> 20,
        bitmap
    );

    // Page allocator: take 16 contiguous pages and give them back.
    let addr = mm::pages(|pa| pa.alloc(16)).expect("page alloc failed");
    let during = mm::pages(|pa| pa.free_pages());
    mm::pages(|pa| pa.free(addr, 16));
    let after = mm::pages(|pa| pa.free_pages());
    if during + 16 != usable || after != usable {
        panic!("page accounting off: {} {} {}", usable, during, after);
    }
    println!("[mm  ] alloc 16 pages at {:#x}, freed", addr);

    // Heap: collections work, and freed memory is reused.
    let v: Vec<u64> = (1..=100_000).collect();
    let sum: u64 = v.iter().sum();
    let first = v.as_ptr() as usize;
    drop(v);
    let v2: Vec<u64> = Vec::with_capacity(100_000);
    let reused = v2.as_ptr() as usize == first;
    drop(v2);
    let b = Box::new([7u8; 64]);
    let s = format!("sum={} reused={} box={}", sum, reused, b[63]);
    println!("[heap] {}", s);
    println!("[heap] {} KiB taken from pages", mm::HEAP.grown_bytes() >> 10);
    if sum != 5_000_050_000 || !reused {
        panic!("heap self-test failed: {}", s);
    }
    ok("page allocator and heap");
}

/// Size of a GICv3 distributor's register frame.
const GICD_SIZE: u64 = 0x1_0000;

fn init_mmu(fb: &Framebuffer, mmap: &impl MemoryMap, gic: Option<&acpi::GicInfo>) {
    let mut m = mmu::Mapper::new();

    // Most specific first: the first mapping of an address wins.
    if let Some(g) = gic {
        if let Some(d) = g.gicd {
            m.map(d, GICD_SIZE, mmu::Kind::Device);
        }
        if let Some((base, len)) = g.gicr {
            m.map(base, len as u64, mmu::Kind::Device);
        }
    }
    let fb_bytes = (fb.stride * fb.height * 4) as u64;
    m.map(fb.base as u64, fb_bytes, mmu::Kind::Uncached);

    for d in mmap.entries() {
        let kind = match d.ty {
            MemoryType::MMIO | MemoryType::MMIO_PORT_SPACE => mmu::Kind::Device,
            // LOADER_CODE holds our whole PE image, data sections included.
            MemoryType::LOADER_CODE | MemoryType::RUNTIME_SERVICES_CODE => mmu::Kind::Code,
            MemoryType::CONVENTIONAL
            | MemoryType::LOADER_DATA
            | MemoryType::BOOT_SERVICES_CODE
            | MemoryType::BOOT_SERVICES_DATA
            | MemoryType::RUNTIME_SERVICES_DATA
            | MemoryType::ACPI_RECLAIM
            | MemoryType::ACPI_NON_VOLATILE
            | MemoryType::PERSISTENT_MEMORY => mmu::Kind::Data,
            _ => continue,
        };
        m.map(d.phys_start, d.page_count * mm::PAGE_SIZE as u64, kind);
    }

    let old = mmu::ttbr0();
    unsafe { m.activate() };
    let st = m.stats();
    println!("[mmu ] TTBR0 {:#x} -> {:#x} (ours)", old, mmu::ttbr0());
    println!(
        "[mmu ] code {} data {} dev {} fb {} MiB",
        st.bytes[1] >> 20,
        st.bytes[0] >> 20,
        st.bytes[2] >> 20,
        st.bytes[3] >> 20
    );
    println!("[mmu ] {} tables, {} blocks, {} pages", st.tables, st.blocks, st.pages);

    // Check through the MMU itself, without risking a fault.
    let code = banner as fn() as usize as u64;
    if mmu::translate(0).is_some() {
        panic!("page 0 is mapped");
    }
    if mmu::translate(code) != Some(code) {
        panic!("kernel code {:#x} is not identity mapped", code);
    }
    ok("M3b: own page tables, null page unmapped");
}

fn report_gic(rsdp: Option<usize>) -> Option<acpi::GicInfo> {
    let Some(rsdp) = rsdp else {
        println!("[gic ] no ACPI, GIC unknown");
        return None;
    };
    match acpi::find_gic(rsdp) {
        Ok(g) => {
            println!(
                "[gic ] GICv{} GICD={:#x} cpus={} its={}",
                g.version,
                g.gicd.unwrap_or(0),
                g.cpus,
                g.its
            );
            match g.gicr {
                Some((base, len)) => println!("[gic ] GICR={:#x} len={:#x}", base, len),
                None => println!("[gic ] no GICR range in MADT"),
            }
            Some(g)
        }
        Err(e) => {
            println!("[gic ] MADT: {}", e);
            None
        }
    }
}

/// Prints the truth table of a small NAND circuit and checks every row.
///
/// Inputs are listed MSB first (`ab`), outputs in the order `build` returns
/// them; `expect` maps the input bits to the expected output bits.
fn truth_table(
    name: &str,
    n_inputs: usize,
    build: impl Fn(&mut Circuit, &[Wire]) -> Vec<Wire>,
    expect: impl Fn(&[bool]) -> Vec<bool>,
) {
    let mut c = Circuit::new();
    let ins: Vec<Wire> = (0..n_inputs).map(|_| c.input()).collect();
    let outs = build(&mut c, &ins);
    let bit = |b: bool| if b { '1' } else { '0' };

    let mut rows = Vec::new();
    for combo in 0..1u32 << n_inputs {
        let bits: Vec<bool> = (0..n_inputs).map(|k| combo >> (n_inputs - 1 - k) & 1 == 1).collect();
        for (&w, &v) in ins.iter().zip(&bits) {
            c.set(w, v);
        }
        c.eval().expect("combinational circuit did not settle");
        let got: Vec<bool> = outs.iter().map(|&w| c.get(w)).collect();
        if got != expect(&bits) {
            panic!("{} is wrong for inputs {:?}", name, bits);
        }
        let mut row: String = bits.iter().map(|&b| bit(b)).collect();
        row.push('>');
        row.extend(got.iter().map(|&b| bit(b)));
        rows.push(row);
    }

    for (i, chunk) in rows.chunks(4).enumerate() {
        let label = if i == 0 { name } else { "" };
        let cells = chunk.join(" ");
        if i == 0 {
            println!("[nand] {:<4} {}  {} NAND", label, cells, c.gate_count());
        } else {
            println!("       {:<4} {}", label, cells);
        }
    }
}

fn nand_demo() {
    truth_table("NOT", 1, |c, i| [c.not(i[0])].into(), |b| [!b[0]].into());
    truth_table("AND", 2, |c, i| [c.and(i[0], i[1])].into(), |b| [b[0] & b[1]].into());
    truth_table("OR", 2, |c, i| [c.or(i[0], i[1])].into(), |b| [b[0] | b[1]].into());
    truth_table("XOR", 2, |c, i| [c.xor(i[0], i[1])].into(), |b| [b[0] ^ b[1]].into());
    truth_table(
        "HA",
        2,
        |c, i| {
            let (s, carry) = c.half_adder(i[0], i[1]);
            [s, carry].into()
        },
        |b| [b[0] ^ b[1], b[0] & b[1]].into(),
    );
    truth_table(
        "FA",
        3,
        |c, i| {
            let (s, carry) = c.full_adder(i[0], i[1], i[2]);
            [s, carry].into()
        },
        |b| {
            let n = b.iter().filter(|&&x| x).count();
            [n & 1 == 1, n >= 2].into()
        },
    );

    // 4-bit ripple-carry adder: show a few sums, then check every input.
    let mut c = Circuit::new();
    let (a, b, cin) = (c.inputs::<4>(), c.inputs::<4>(), c.input());
    let (sum, cout) = c.adder(a, b, cin);
    let mut add = |x: u64, y: u64, carry_in: bool| {
        c.set_bits(&a, x);
        c.set_bits(&b, y);
        c.set(cin, carry_in);
        c.eval().expect("adder did not settle");
        c.get_bits(&sum) | (c.get(cout) as u64) << 4
    };
    for (x, y) in [(3, 5), (7, 8), (9, 9), (15, 15)] {
        let r = add(x, y, false);
        println!("[nand] {:2} + {:2} = {:2}  ({:04b} + {:04b} = {:05b})", x, y, r, x, y, r);
    }
    for x in 0..16 {
        for y in 0..16 {
            for carry_in in [false, true] {
                let r = add(x, y, carry_in);
                if r != x + y + carry_in as u64 {
                    panic!("4-bit adder: {} + {} + {} gave {}", x, y, carry_in as u8, r);
                }
            }
        }
    }
    let gates = c.gate_count();
    ok(format_args!("M2.5: 4-bit adder 512/512 ({} NAND)", gates));
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
