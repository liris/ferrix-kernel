//! Bring-up of each subsystem at boot, in the order `kernel_main` calls
//! them, with a self-test and a line on screen for each.

use crate::arch::{self, exception};
use crate::dev::console::Framebuffer;
use crate::dev::usb::{self, xhci};
use crate::dev::{acpi, gic, pci};
use crate::mm::{self, mmu};
use crate::proc::user;
use crate::{ok, println};
use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;
use uefi::boot::MemoryType;
use uefi::mem::memory_map::MemoryMap;

/// Checks that the kernel runs at EL1, installs the exception vectors and
/// takes a `brk` and an `svc` through them.
pub fn exceptions() {
    if arch::current_el() != 1 {
        panic!("expected EL1, running at EL{}", arch::current_el());
    }
    let vbar = exception::init();
    ok(format_args!("vectors at VBAR_EL1={:#x}", vbar));

    // Round-trip through both handled exception types.
    // SAFETY: the handler skips the `brk` and resumes here.
    unsafe { core::arch::asm!("brk #0x42", clobber_abi("C")) };
    let ret: u64;
    // SAFETY: the handler adds 1 to x0 and resumes after the `svc`.
    unsafe { core::arch::asm!("svc #0x7", inout("x0") 41u64 => ret, clobber_abi("C")) };
    if ret != 42 {
        panic!("svc returned {} instead of 42", ret);
    }
    ok("brk/svc handled and resumed");
}

/// Sets up the page allocator and heap from the memory map, and tests both.
pub fn memory(mmap: &impl MemoryMap) {
    mm::init(mmap);
    let (free, usable, bitmap) =
        mm::pages(|pa| (pa.free_pages(), pa.usable_pages(), pa.bitmap_addr()));
    println!(
        "[mm  ] {} MiB free in 4K pages, bitmap at {:#x}",
        (free * mm::PAGE_SIZE) >> 20,
        bitmap
    );

    // Page allocator: take 16 contiguous pages and give them back.
    let addr = mm::pages(|pa| pa.alloc(16)).expect("page alloc failed");
    let during = mm::pages(|pa| pa.free_pages());
    // SAFETY: allocated just above and not used.
    mm::pages(|pa| unsafe { pa.free(addr, 16) });
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

/// Builds the kernel's own page tables, identity mapping RAM, MMIO, the
/// framebuffer and the GIC, and switches to them.
pub fn page_tables(fb: &Framebuffer, mmap: &impl MemoryMap, gic: Option<&acpi::GicInfo>) {
    let mut m = mmu::Mapper::new();

    // Most specific first: the first mapping of an address wins.
    if let Some(g) = gic {
        if let Some(d) = g.gicd {
            m.map(d, gic::GICD_SIZE, mmu::Kind::Device);
        }
        if let Some((base, len)) = g.gicr {
            m.map(base, len as u64, mmu::Kind::Device);
        }
    }
    m.map(fb.addr(), fb.bytes(), mmu::Kind::Uncached);

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
    let st = m.stats();
    // SAFETY: everything the kernel touches is mapped above: all RAM in the memory map
    // (code, stacks, heap, page tables, ACPI), the framebuffer, the GIC, and MMIO. Regions
    // found later go through `map_live`.
    unsafe { m.activate() };
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
    let code = crate::banner as fn() as usize as u64;
    if mmu::translate(0).is_some() {
        panic!("page 0 is mapped");
    }
    if mmu::translate(code) != Some(code) {
        panic!("kernel code {:#x} is not identity mapped", code);
    }
    ok("M3b: own page tables, null page unmapped");
}

/// Finds the GIC in the MADT and reports it.
pub fn find_gic(rsdp: Option<acpi::Rsdp>) -> Option<acpi::GicInfo> {
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

/// PCI class code of an xHCI controller: serial bus, USB, xHCI.
const CLASS_XHCI: [u8; 3] = [0x0c, 0x03, 0x30];

/// Finds the xHCI controller and sets up USB. Returns whether a keyboard
/// was found.
pub fn usb(rsdp: Option<acpi::Rsdp>) -> bool {
    let ecam = match rsdp.map(acpi::find_ecam) {
        Some(Ok(e)) => e,
        Some(Err(e)) => {
            println!("[pci ] MCFG: {}", e);
            return false;
        }
        None => {
            println!("[pci ] no ACPI, no PCIe");
            return false;
        }
    };
    println!(
        "[pci ] ECAM {:#x} seg {} bus {}-{}",
        ecam.base, ecam.segment, ecam.bus_start, ecam.bus_end
    );
    // SAFETY: `ecam` came from `acpi::find_ecam`.
    let Some(f) = (unsafe { pci::find_class(&ecam, CLASS_XHCI) }) else {
        println!("[pci ] no xHCI controller");
        return false;
    };
    let (vendor, device) = f.ids();
    println!(
        "[pci ] {:02x}:{:02x}.{} xHCI {:04x}:{:04x}",
        f.bus, f.device, f.function, vendor, device
    );
    let hc = match xhci::Xhci::init(&f) {
        Ok(hc) => hc,
        Err(e) => {
            println!("[xhci] {}", e);
            return false;
        }
    };
    println!(
        "[xhci] v{:x}.{:02x} slots {} ports {}, running",
        hc.version >> 8,
        hc.version & 0xff,
        hc.max_slots,
        hc.max_ports
    );
    let found = usb::init(hc);
    if found {
        ok("M5: USB keyboard ready");
    } else {
        println!("[usb ] no keyboard");
    }
    found
}

/// Loads the user programs and starts them at EL0. Returns whether they
/// run (and with them the circuit builder).
pub fn user_programs() -> bool {
    match user::start() {
        Ok((n, caps)) => {
            println!("[user] {} programs at EL0, {} KiB ELF", n, user::elf_size() >> 10);
            ok(format_args!("M7: user space, {} address spaces", n));
            ok(format_args!("M8: {} capabilities, 2 channels", caps));
            ok("M9: circuit builder u6 has the keyboard");
            true
        }
        Err(e) => {
            println!("[user] {}", e);
            false
        }
    }
}
