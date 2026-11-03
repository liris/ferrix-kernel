//! xHCI USB host controller, driven by polling.
//!
//! VMware's ARM VMs have no PS/2, so the keyboard is a USB device behind an
//! xHCI controller; QEMU runs `qemu-xhci` + `usb-kbd` to match. The firmware
//! used the controller until ExitBootServices, so it is taken over (legacy
//! handoff), halted and reset before use.
//!
//! Register blocks, all found from the capability registers at BAR0:
//! operational (`op`: commands, rings, per-port status), runtime (`ir`:
//! interrupter 0, which owns the event ring) and doorbells (`db`).
//!
//! The controller and the kernel talk through rings of 16-byte TRBs in RAM:
//! we produce on the command ring and on one transfer ring per endpoint, and
//! consume the event ring. A cycle bit in every TRB tells the consumer which
//! entries are new; it flips each time a ring wraps. Interrupts stay off: the
//! event ring is polled. RAM is identity mapped and cacheable, and DMA on
//! both hypervisors is cache coherent, so barriers order the CPU's accesses
//! and no cache maintenance is needed.

use crate::{arch, mm, pci, task};
use core::arch::asm;
use core::ptr::{read_volatile, write_bytes, write_volatile};

// Capability registers.
/// CAPLENGTH in the low byte, HCIVERSION in the high half.
const CAPLENGTH: usize = 0x00;
const HCSPARAMS1: usize = 0x04;
const HCSPARAMS2: usize = 0x08;
const HCCPARAMS1: usize = 0x10;
const DBOFF: usize = 0x14;
const RTSOFF: usize = 0x18;

// Operational registers.
const USBCMD: usize = 0x00;
const USBSTS: usize = 0x04;
const CRCR: usize = 0x18;
const DCBAAP: usize = 0x30;
const CONFIG: usize = 0x38;
const PORTSC: usize = 0x400;
const PORT_STRIDE: usize = 0x10;

const USBCMD_RUN: u32 = 1 << 0;
const USBCMD_RESET: u32 = 1 << 1;
const USBSTS_HALTED: u32 = 1 << 0;
const USBSTS_NOT_READY: u32 = 1 << 11;
const HCCPARAMS1_CSZ: u32 = 1 << 2;

const PORTSC_CONNECTED: u32 = 1 << 0;
const PORTSC_ENABLED: u32 = 1 << 1;
const PORTSC_RESET: u32 = 1 << 4;
const PORTSC_POWER: u32 = 1 << 9;
const PORTSC_RESET_CHANGE: u32 = 1 << 21;
/// Write-1-to-clear change bits (connect, enable, ... reset, link, config).
const PORTSC_CHANGES: u32 = 0x7f << 17;
/// Bits that must be written back unchanged: power and the wake enables.
/// Writing 1 to ENABLED would disable the port, so it is never kept.
const PORTSC_KEEP: u32 = PORTSC_POWER | 0x7 << 25;

// Runtime registers of interrupter 0.
const IR0: usize = 0x20;
const ERSTSZ: usize = 0x08;
const ERSTBA: usize = 0x10;
const ERDP: usize = 0x18;
const ERDP_BUSY: u64 = 1 << 3;

// Extended capabilities.
const XCAP_LEGACY: u32 = 1;
const LEGACY_BIOS_OWNED: u32 = 1 << 16;
const LEGACY_OS_OWNED: u32 = 1 << 24;

// TRB types (control word bits 15:10) and flags.
const TRB_NORMAL: u32 = 1;
const TRB_SETUP: u32 = 2;
const TRB_DATA: u32 = 3;
const TRB_STATUS: u32 = 4;
const TRB_LINK: u32 = 6;
const TRB_ENABLE_SLOT: u32 = 9;
const TRB_ADDRESS_DEVICE: u32 = 11;
const TRB_CONFIGURE_ENDPOINT: u32 = 12;
const TRB_EVALUATE_CONTEXT: u32 = 13;
const EVENT_TRANSFER: u32 = 32;
const EVENT_COMMAND: u32 = 33;

const TRB_CYCLE: u32 = 1 << 0;
const LINK_TOGGLE_CYCLE: u32 = 1 << 1;
const TRB_SHORT_OK: u32 = 1 << 2;
const TRB_IOC: u32 = 1 << 5;
const TRB_IMMEDIATE: u32 = 1 << 6;
const TRB_DIR_IN: u32 = 1 << 16;
const SETUP_IN_DATA: u32 = 3 << 16;

const COMPLETION_SUCCESS: u32 = 1;
const COMPLETION_SHORT_PACKET: u32 = 13;

// Context fields.
const ADD_SLOT: u32 = 1 << 0;
const ADD_EP0: u32 = 1 << 1;
const EP_CONTROL: u32 = 4;
const EP_INTERRUPT_IN: u32 = 7;
/// Retry a failing transaction up to 3 times before reporting an error.
const EP_ERROR_COUNT: u32 = 3 << 1;

// Standard control requests.
pub const REQ_IN: u8 = 0x80;
pub const REQ_GET_DESCRIPTOR: u8 = 6;
pub const DESC_DEVICE: u16 = 1;

/// How long the controller may take to halt, reset or answer a command.
const TIMEOUT_MS: u64 = 1000;

type Trb = [u32; 4];

fn read32(addr: usize) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}

fn write32(addr: usize, v: u32) {
    unsafe { write_volatile(addr as *mut u32, v) }
}

/// 64-bit registers are written as two dwords, low first: QEMU only accepts
/// 32-bit accesses and acts on ERSTBA when its high half is written.
fn write64(addr: usize, v: u64) {
    write32(addr, v as u32);
    write32(addr + 4, (v >> 32) as u32);
}

/// Zeroed, page-aligned memory for the controller to read and write.
fn dma_pages(count: usize) -> usize {
    let addr = mm::pages(|pa| pa.alloc(count)).expect("out of memory for xHCI");
    unsafe { write_bytes(addr as *mut u8, 0, count * mm::PAGE_SIZE) };
    addr
}

/// Polls `cond` until it holds or `ms` milliseconds pass, letting other
/// tasks run meanwhile. Returns whether it held.
fn wait_for(ms: u64, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = arch::timer_count() + arch::timer_freq() * ms / 1000;
    while !cond() {
        if arch::timer_count() > deadline {
            return false;
        }
        task::yield_now();
    }
    true
}

fn trb_type(t: &Trb) -> u32 {
    (t[3] >> 10) & 0x3f
}

fn trb_pointer(t: &Trb) -> u64 {
    (t[1] as u64) << 32 | t[0] as u64
}

fn completion_ok(t: &Trb) -> bool {
    matches!(t[2] >> 24, COMPLETION_SUCCESS | COMPLETION_SHORT_PACKET)
}

const RING_TRBS: usize = mm::PAGE_SIZE / 16;

/// A ring we produce on (commands or one endpoint's transfers): one page,
/// closed by a Link TRB back to its start.
struct Ring {
    base: usize,
    next: usize,
    cycle: u32,
}

impl Ring {
    fn new() -> Self {
        Self { base: dma_pages(1), next: 0, cycle: TRB_CYCLE }
    }

    /// Writes `trb` with the current cycle bit and returns its address.
    fn push(&mut self, trb: Trb) -> u64 {
        let at = self.base + self.next * 16;
        self.write(at, trb);
        self.next += 1;
        if self.next == RING_TRBS - 1 {
            let base = self.base as u64;
            let link = [base as u32, (base >> 32) as u32, 0, TRB_LINK << 10 | LINK_TOGGLE_CYCLE];
            self.write(self.base + self.next * 16, link);
            self.next = 0;
            self.cycle ^= TRB_CYCLE;
        }
        at as u64
    }

    /// The cycle bit goes in last, so the controller never sees half a TRB.
    fn write(&self, at: usize, trb: Trb) {
        let p = at as *mut u32;
        unsafe {
            for (i, &v) in trb[..3].iter().enumerate() {
                write_volatile(p.add(i), v);
            }
            asm!("dmb ishst", options(nostack));
            write_volatile(p.add(3), trb[3] | self.cycle);
        }
    }
}

/// The ring the controller produces events on: one segment of one page.
struct EventRing {
    base: usize,
    next: usize,
    cycle: u32,
}

impl EventRing {
    fn pop(&mut self, ir: usize) -> Option<Trb> {
        let p = (self.base + self.next * 16) as *const u32;
        if unsafe { read_volatile(p.add(3)) } & TRB_CYCLE != self.cycle {
            return None;
        }
        // Read the rest only after seeing the cycle bit.
        unsafe { asm!("dmb ishld", options(nostack)) };
        let trb: Trb = core::array::from_fn(|i| unsafe { read_volatile(p.add(i)) });
        self.next += 1;
        if self.next == RING_TRBS {
            self.next = 0;
            self.cycle ^= TRB_CYCLE;
        }
        write64(ir + ERDP, (self.base + self.next * 16) as u64 | ERDP_BUSY);
        Some(trb)
    }
}

pub struct Port {
    pub number: u8,
    /// Speed ID from PORTSC: 1 full, 2 low, 3 high, 4 super. Only valid
    /// once the port is enabled.
    pub speed: u8,
}

pub fn speed_name(speed: u8) -> &'static str {
    match speed {
        1 => "full",
        2 => "low",
        3 => "high",
        4 => "super",
        5 => "super+",
        _ => "?",
    }
}

/// An addressed device: its slot, input context and default control
/// endpoint.
pub struct Device {
    pub slot: u8,
    pub port: u8,
    pub speed: u8,
    input: usize,
    ep0: Ring,
    max_packet0: u16,
    /// One page for control transfer data.
    buf: usize,
}

impl Device {
    /// The first `len` bytes of the last control transfer's data.
    pub fn data(&self, len: usize) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.buf as *const u8, len.min(mm::PAGE_SIZE)) }
    }
}

/// Transfers an interrupt IN endpoint can have queued at once.
///
/// With only one, there is a window after each completion, until the next
/// `submit`, in which nothing is queued; QEMU's keyboard was seen to stop
/// reporting for good when keys came in that window. Keeping several queued
/// closes it.
pub const IN_FLIGHT: usize = 4;

/// A configured interrupt IN endpoint with `IN_FLIGHT` buffers of `len`
/// bytes each.
pub struct InterruptIn {
    slot: u8,
    dci: u32,
    ring: Ring,
    buf: usize,
    len: u32,
    /// The buffer the next completion belongs to: transfers on one ring
    /// complete in the order they were queued.
    done: usize,
}

impl InterruptIn {
    pub fn data(&self, index: usize, len: usize) -> &[u8] {
        let at = self.buf + index * self.len as usize;
        unsafe { core::slice::from_raw_parts(at as *const u8, len.min(self.len as usize)) }
    }
}

pub struct Xhci {
    cap: usize,
    op: usize,
    ir: usize,
    db: usize,
    ctx_size: usize,
    dcbaa: *mut u64,
    cmd: Ring,
    events: EventRing,
    pub version: u16,
    pub max_slots: u8,
    pub max_ports: u8,
}

impl Xhci {
    /// Takes the controller from the firmware, resets it, sets up the
    /// command and event rings and starts it.
    pub fn init(f: &pci::Function) -> Result<Self, &'static str> {
        let (base, _) = f.map_bar(0).ok_or("BAR0 not assigned")?;
        f.enable_memory_and_dma();
        let cap = base as usize;
        // Read as one dword: QEMU only accepts 32-bit accesses here.
        let first = read32(cap + CAPLENGTH);
        let params1 = read32(cap + HCSPARAMS1);
        let hc = Self {
            cap,
            op: cap + (first as u8 as usize),
            ir: cap + (read32(cap + RTSOFF) & !0x1f) as usize + IR0,
            db: cap + (read32(cap + DBOFF) & !0x3) as usize,
            ctx_size: if read32(cap + HCCPARAMS1) & HCCPARAMS1_CSZ != 0 { 64 } else { 32 },
            dcbaa: dma_pages(1) as *mut u64,
            cmd: Ring::new(),
            events: EventRing { base: dma_pages(1), next: 0, cycle: TRB_CYCLE },
            version: (first >> 16) as u16,
            max_slots: params1 as u8,
            max_ports: (params1 >> 24) as u8,
        };

        hc.take_ownership()?;
        hc.reset()?;

        // Scratchpad pages the controller may ask for, listed in DCBAA[0].
        let params2 = read32(cap + HCSPARAMS2);
        let scratch = ((params2 >> 21) & 0x1f) << 5 | params2 >> 27;
        if scratch > 0 {
            let list = dma_pages((scratch as usize * 8).div_ceil(mm::PAGE_SIZE)) as *mut u64;
            for i in 0..scratch as usize {
                unsafe { *list.add(i) = dma_pages(1) as u64 };
            }
            unsafe { *hc.dcbaa = list as u64 };
        }

        let config = read32(hc.op + CONFIG) & !0xff;
        write32(hc.op + CONFIG, config | hc.max_slots as u32);
        write64(hc.op + DCBAAP, hc.dcbaa as u64);
        write64(hc.op + CRCR, hc.cmd.base as u64 | TRB_CYCLE as u64);

        // One event ring segment table entry: (base, size in TRBs).
        let erst = dma_pages(1) as *mut u64;
        unsafe {
            *erst = hc.events.base as u64;
            *erst.add(1) = RING_TRBS as u64;
        }
        write32(hc.ir + ERSTSZ, 1);
        write64(hc.ir + ERDP, hc.events.base as u64);
        write64(hc.ir + ERSTBA, erst as u64);

        unsafe { asm!("dsb sy", options(nostack)) };
        write32(hc.op + USBCMD, USBCMD_RUN);
        let op = hc.op;
        if !wait_for(TIMEOUT_MS, || read32(op + USBSTS) & USBSTS_HALTED == 0) {
            return Err("controller did not start");
        }
        Ok(hc)
    }

    /// Walks the extended capability list and, if the firmware still owns
    /// the controller through the legacy support capability, asks for it.
    fn take_ownership(&self) -> Result<(), &'static str> {
        let mut off = (read32(self.cap + HCCPARAMS1) >> 16) as usize * 4;
        while off != 0 {
            let addr = self.cap + off;
            let v = read32(addr);
            if v & 0xff == XCAP_LEGACY && v & LEGACY_BIOS_OWNED != 0 {
                write32(addr, v | LEGACY_OS_OWNED);
                if !wait_for(TIMEOUT_MS, || read32(addr) & LEGACY_BIOS_OWNED == 0) {
                    return Err("firmware did not release the controller");
                }
            }
            off = match (v >> 8) & 0xff {
                0 => 0,
                next => off + next as usize * 4,
            };
        }
        Ok(())
    }

    fn reset(&self) -> Result<(), &'static str> {
        let op = self.op;
        write32(op + USBCMD, read32(op + USBCMD) & !USBCMD_RUN);
        if !wait_for(TIMEOUT_MS, || read32(op + USBSTS) & USBSTS_HALTED != 0) {
            return Err("controller did not halt");
        }
        write32(op + USBCMD, USBCMD_RESET);
        if !wait_for(TIMEOUT_MS, || {
            read32(op + USBCMD) & USBCMD_RESET == 0 && read32(op + USBSTS) & USBSTS_NOT_READY == 0
        }) {
            return Err("controller did not come out of reset");
        }
        Ok(())
    }

    fn portsc(&self, port: u8) -> usize {
        self.op + PORTSC + (port as usize - 1) * PORT_STRIDE
    }

    /// Numbers of the root hub ports with something plugged in.
    pub fn connected_ports(&self) -> impl Iterator<Item = u8> + '_ {
        (1..=self.max_ports).filter(|&n| read32(self.portsc(n)) & PORTSC_CONNECTED != 0)
    }

    /// Resets a port unless it is already enabled (USB 3 ports enable
    /// themselves), which also makes its speed known.
    pub fn enable_port(&self, number: u8) -> Result<Port, &'static str> {
        let reg = self.portsc(number);
        let sc = read32(reg);
        if sc & PORTSC_ENABLED == 0 {
            write32(reg, sc & PORTSC_KEEP | PORTSC_RESET);
            if !wait_for(TIMEOUT_MS, || read32(reg) & PORTSC_RESET_CHANGE != 0) {
                return Err("port reset timed out");
            }
            // USB 2.0 gives a device 10 ms to recover after reset.
            wait_for(10, || false);
        }
        write32(reg, read32(reg) & PORTSC_KEEP | PORTSC_CHANGES);
        let sc = read32(reg);
        if sc & PORTSC_ENABLED == 0 {
            return Err("port did not enable");
        }
        Ok(Port { number, speed: ((sc >> 10) & 0xf) as u8 })
    }

    fn ring_doorbell(&self, slot: u8, target: u32) {
        // TRBs written to RAM before the MMIO write that announces them.
        unsafe { asm!("dsb sy", options(nostack)) };
        write32(self.db + slot as usize * 4, target);
    }

    /// Polls the event ring until `want` matches an event; other events
    /// (port status changes) are dropped.
    fn wait_event(&mut self, want: impl Fn(&Trb) -> bool) -> Result<Trb, &'static str> {
        let (events, ir) = (&mut self.events, self.ir);
        let mut found = None;
        wait_for(TIMEOUT_MS, || {
            while let Some(e) = events.pop(ir) {
                if want(&e) {
                    found = Some(e);
                    return true;
                }
            }
            false
        });
        found.ok_or("no answer from the controller")
    }

    /// Runs one command; returns the slot ID from its completion.
    fn command(&mut self, trb: Trb) -> Result<u8, &'static str> {
        let at = self.cmd.push(trb);
        self.ring_doorbell(0, 0);
        let e = self.wait_event(|e| trb_type(e) == EVENT_COMMAND && trb_pointer(e) == at)?;
        if e[2] >> 24 != COMPLETION_SUCCESS {
            return Err("command failed");
        }
        Ok((e[3] >> 24) as u8)
    }

    fn context(&self, base: usize, index: usize) -> *mut u32 {
        (base + index * self.ctx_size) as *mut u32
    }

    /// Fills the device's input context: control (`add` flags), slot (with
    /// contexts up to index `entries` in use) and endpoint 0.
    fn prepare_input(&self, dev: &Device, add: u32, entries: u32) {
        unsafe {
            write_bytes(dev.input as *mut u8, 0, mm::PAGE_SIZE);
            let control = self.context(dev.input, 0);
            *control.add(1) = add;
            let slot = self.context(dev.input, 1);
            *slot = (dev.speed as u32) << 20 | entries << 27;
            *slot.add(1) = (dev.port as u32) << 16;
            let ep0 = self.context(dev.input, 2);
            *ep0.add(1) = EP_ERROR_COUNT | EP_CONTROL << 3 | (dev.max_packet0 as u32) << 16;
            let ring = dev.ep0.base as u64 | TRB_CYCLE as u64;
            *ep0.add(2) = ring as u32;
            *ep0.add(3) = (ring >> 32) as u32;
            *ep0.add(4) = 8; // average TRB length: setup packets
        }
    }

    fn input_command(&mut self, dev: &Device, ty: u32) -> Result<u8, &'static str> {
        let input = dev.input as u64;
        self.command([input as u32, (input >> 32) as u32, 0, (dev.slot as u32) << 24 | ty << 10])
    }

    /// Gives the device on an enabled port a slot and an address, and reads
    /// the first 8 bytes of its device descriptor to learn the control
    /// endpoint's packet size.
    pub fn address_device(&mut self, port: &Port) -> Result<Device, &'static str> {
        let slot = self.command([0, 0, 0, TRB_ENABLE_SLOT << 10])?;
        let mut dev = Device {
            slot,
            port: port.number,
            speed: port.speed,
            input: dma_pages(1),
            ep0: Ring::new(),
            // Until the descriptor says otherwise: the minimum per speed.
            max_packet0: match port.speed {
                3 => 64,
                4.. => 512,
                _ => 8,
            },
            buf: dma_pages(1),
        };
        unsafe { *self.dcbaa.add(slot as usize) = dma_pages(1) as u64 };
        self.prepare_input(&dev, ADD_SLOT | ADD_EP0, 1);
        self.input_command(&dev, TRB_ADDRESS_DEVICE)?;

        self.control(&mut dev, REQ_IN, REQ_GET_DESCRIPTOR, DESC_DEVICE << 8, 0, 8)?;
        let size = dev.data(8)[7];
        let max_packet0 = if dev.speed >= 4 { 1 << size } else { size as u16 };
        if max_packet0 != dev.max_packet0 && max_packet0 != 0 {
            dev.max_packet0 = max_packet0;
            self.prepare_input(&dev, ADD_EP0, 1);
            self.input_command(&dev, TRB_EVALUATE_CONTEXT)?;
        }
        Ok(dev)
    }

    /// A control transfer on endpoint 0, either with `len` bytes of IN data
    /// (read them with `Device::data`) or with no data stage.
    pub fn control(
        &mut self,
        dev: &mut Device,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        len: u16,
    ) -> Result<(), &'static str> {
        debug_assert!(len == 0 || request_type & REQ_IN != 0);
        let setup0 = request_type as u32 | (request as u32) << 8 | (value as u32) << 16;
        let setup1 = index as u32 | (len as u32) << 16;
        let data = if len > 0 { SETUP_IN_DATA } else { 0 };
        dev.ep0.push([setup0, setup1, 8, TRB_SETUP << 10 | TRB_IMMEDIATE | data]);
        if len > 0 {
            let buf = dev.buf as u64;
            dev.ep0.push([buf as u32, (buf >> 32) as u32, len as u32, TRB_DATA << 10 | TRB_DIR_IN]);
        }
        // The status stage goes the other way from the data (IN if none).
        let status_dir = if len > 0 { 0 } else { TRB_DIR_IN };
        dev.ep0.push([0, 0, 0, TRB_STATUS << 10 | TRB_IOC | status_dir]);
        self.ring_doorbell(dev.slot, 1);

        let slot = dev.slot as u32;
        let e = self.wait_event(|e| trb_type(e) == EVENT_TRANSFER && e[3] >> 24 == slot)?;
        if !completion_ok(&e) {
            return Err("control transfer failed");
        }
        Ok(())
    }

    /// Adds an interrupt IN endpoint (address `ep`, packet size `max_packet`,
    /// `interval` as in its endpoint descriptor) to a configured device.
    pub fn configure_interrupt_in(
        &mut self,
        dev: &Device,
        ep: u8,
        max_packet: u16,
        interval: u8,
    ) -> Result<InterruptIn, &'static str> {
        // Device context index: OUT endpoints are 2n, IN endpoints 2n + 1.
        let dci = (ep as u32 & 0xf) * 2 + 1;
        let ring = Ring::new();
        // xHCI wants the period as 2^n microframes (125 us); full and low
        // speed descriptors give milliseconds, high and super speed 2^(n-1).
        let period = if dev.speed >= 3 {
            interval.clamp(1, 16) as u32 - 1
        } else {
            (interval.max(1) as u32 * 8).ilog2().clamp(3, 10)
        };
        self.prepare_input(dev, ADD_SLOT | 1 << dci, dci);
        unsafe {
            let ctx = self.context(dev.input, 1 + dci as usize);
            *ctx = period << 16;
            *ctx.add(1) = EP_ERROR_COUNT | EP_INTERRUPT_IN << 3 | (max_packet as u32) << 16;
            let tr = ring.base as u64 | TRB_CYCLE as u64;
            *ctx.add(2) = tr as u32;
            *ctx.add(3) = (tr >> 32) as u32;
            // Average TRB length and max payload per service interval.
            *ctx.add(4) = max_packet as u32 | (max_packet as u32) << 16;
        }
        self.input_command(dev, TRB_CONFIGURE_ENDPOINT)?;
        // Interrupt endpoints carry at most 1024 bytes per packet.
        let buf = dma_pages((IN_FLIGHT * max_packet as usize).div_ceil(mm::PAGE_SIZE));
        Ok(InterruptIn { slot: dev.slot, dci, ring, buf, len: max_packet as u32, done: 0 })
    }

    /// Queues one transfer into buffer `index` of the endpoint.
    pub fn submit(&mut self, ep: &mut InterruptIn, index: usize) {
        let buf = (ep.buf + index * ep.len as usize) as u64;
        let flags = TRB_NORMAL << 10 | TRB_IOC | TRB_SHORT_OK;
        ep.ring.push([buf as u32, (buf >> 32) as u32, ep.len, flags]);
        self.ring_doorbell(ep.slot, ep.dci);
    }

    /// Checks for the next completed transfer: which buffer, and the number
    /// of bytes received or an error. `None` while none has completed.
    pub fn poll(&mut self, ep: &mut InterruptIn) -> Option<(usize, Result<usize, &'static str>)> {
        while let Some(e) = self.events.pop(self.ir) {
            let mine = e[3] >> 24 == ep.slot as u32 && (e[3] >> 16) & 0x1f == ep.dci;
            if trb_type(&e) == EVENT_TRANSFER && mine {
                let index = ep.done;
                ep.done = (ep.done + 1) % IN_FLIGHT;
                if !completion_ok(&e) {
                    return Some((index, Err("interrupt transfer failed")));
                }
                // The event reports how many bytes were *not* transferred.
                return Some((index, Ok(ep.len.saturating_sub(e[2] & 0xff_ffff) as usize)));
            }
        }
        None
    }
}
