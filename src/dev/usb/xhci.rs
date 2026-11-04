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

use crate::dev::pci;
use crate::mmio::Region;
use crate::proc::task;
use crate::{arch, mm};
use core::arch::asm;

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
const IR_SIZE: usize = 0x20;
/// Doorbell 0 (the host's) plus one per slot.
const DOORBELLS: usize = 256;
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

/// 64-bit registers are written as two dwords, low first: QEMU only accepts
/// 32-bit accesses and acts on ERSTBA when its high half is written.
fn write64(r: &Region, off: usize, v: u64) {
    r.write32(off, v as u32);
    r.write32(off + 4, (v >> 32) as u32);
}

/// Orders the CPU's memory writes before what follows (an MMIO write, or
/// the TRB's cycle bit).
fn dsb() {
    // SAFETY: a barrier only.
    unsafe { asm!("dsb sy", options(nostack)) };
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
    mem: Region,
    next: usize,
    cycle: u32,
}

impl Ring {
    fn new() -> Self {
        Self { mem: Region::dma(1), next: 0, cycle: TRB_CYCLE }
    }

    /// Writes `trb` with the current cycle bit and returns its address.
    fn push(&mut self, trb: Trb) -> u64 {
        let at = self.next * 16;
        self.write(at, trb);
        self.next += 1;
        if self.next == RING_TRBS - 1 {
            let base = self.mem.addr();
            let link = [base as u32, (base >> 32) as u32, 0, TRB_LINK << 10 | LINK_TOGGLE_CYCLE];
            self.write(self.next * 16, link);
            self.next = 0;
            self.cycle ^= TRB_CYCLE;
        }
        self.mem.addr() + at as u64
    }

    /// The cycle bit goes in last, so the controller never sees half a TRB.
    fn write(&self, at: usize, trb: Trb) {
        for (i, &v) in trb[..3].iter().enumerate() {
            self.mem.write32(at + i * 4, v);
        }
        // SAFETY: a barrier only.
        unsafe { asm!("dmb ishst", options(nostack)) };
        self.mem.write32(at + 12, trb[3] | self.cycle);
    }
}

/// The ring the controller produces events on: one segment of one page.
struct EventRing {
    mem: Region,
    next: usize,
    cycle: u32,
}

impl EventRing {
    fn pop(&mut self, ir: &Region) -> Option<Trb> {
        let at = self.next * 16;
        if self.mem.read32(at + 12) & TRB_CYCLE != self.cycle {
            return None;
        }
        // Read the rest only after seeing the cycle bit.
        // SAFETY: a barrier only.
        unsafe { asm!("dmb ishld", options(nostack)) };
        let trb: Trb = core::array::from_fn(|i| self.mem.read32(at + i * 4));
        self.next += 1;
        if self.next == RING_TRBS {
            self.next = 0;
            self.cycle ^= TRB_CYCLE;
        }
        write64(ir, ERDP, (self.mem.addr() + self.next as u64 * 16) | ERDP_BUSY);
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
    input: Region,
    ep0: Ring,
    max_packet0: u16,
    /// One page for control transfer data.
    buf: Region,
}

impl Device {
    /// The first `len` bytes of the last control transfer's data.
    pub fn data(&self, len: usize) -> &[u8] {
        // SAFETY: the controller writes `buf` only during `Xhci::control`, which takes
        // `&mut Device` and waits for the transfer to finish, so not while this borrow lives.
        unsafe { self.buf.bytes(0, len) }
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
    buf: Region,
    len: u32,
    /// The buffer the next completion belongs to: transfers on one ring
    /// complete in the order they were queued.
    done: usize,
}

impl InterruptIn {
    /// The first `len` bytes received into buffer `index`.
    ///
    /// # Safety
    ///
    /// That buffer's transfer must have completed (`Xhci::poll` returned it) and not been
    /// submitted again, so the controller isn't writing to it.
    pub unsafe fn data(&self, index: usize, len: usize) -> &[u8] {
        let size = self.len as usize;
        // SAFETY: the controller is done with this buffer (caller).
        unsafe { self.buf.bytes(index * size, len.min(size)) }
    }
}

pub struct Xhci {
    /// BAR0; capability registers at its start.
    cap: Region,
    op: Region,
    ir: Region,
    db: Region,
    ctx_size: usize,
    dcbaa: Region,
    cmd: Ring,
    events: EventRing,
    pub version: u16,
    pub max_slots: u8,
    pub max_ports: u8,
}

// SAFETY: the registers and DMA memory belong to this controller alone; whoever holds the
// `Xhci` is the only one driving it.
unsafe impl Send for Xhci {}

impl Xhci {
    /// Takes the controller from the firmware, resets it, sets up the
    /// command and event rings and starts it.
    pub fn init(f: &pci::Function) -> Result<Self, &'static str> {
        let (base, size) = f.map_bar(0).ok_or("BAR0 not assigned")?;
        f.enable_memory_and_dma();
        // SAFETY: BAR0 is this controller's register block, as the firmware assigned it,
        // and `map_bar` mapped it as device memory.
        let cap = unsafe { Region::new(base as usize, size as usize) };
        // Read as one dword: QEMU only accepts 32-bit accesses here.
        let first = cap.read32(CAPLENGTH);
        let params1 = cap.read32(HCSPARAMS1);
        let op = first as u8 as usize;
        let ir = (cap.read32(RTSOFF) & !0x1f) as usize + IR0;
        let db = (cap.read32(DBOFF) & !0x3) as usize;
        let hc = Self {
            cap,
            op: cap.sub(op, PORTSC + (params1 >> 24) as usize * PORT_STRIDE),
            ir: cap.sub(ir, IR_SIZE),
            db: cap.sub(db, DOORBELLS * 4),
            ctx_size: if cap.read32(HCCPARAMS1) & HCCPARAMS1_CSZ != 0 { 64 } else { 32 },
            dcbaa: Region::dma(1),
            cmd: Ring::new(),
            events: EventRing { mem: Region::dma(1), next: 0, cycle: TRB_CYCLE },
            version: (first >> 16) as u16,
            max_slots: params1 as u8,
            max_ports: (params1 >> 24) as u8,
        };

        hc.take_ownership()?;
        hc.reset()?;

        // Scratchpad pages the controller may ask for, listed in DCBAA[0].
        let params2 = cap.read32(HCSPARAMS2);
        let scratch = ((params2 >> 21) & 0x1f) << 5 | params2 >> 27;
        if scratch > 0 {
            let list = Region::dma((scratch as usize * 8).div_ceil(mm::PAGE_SIZE));
            for i in 0..scratch as usize {
                list.write64(i * 8, Region::dma(1).addr());
            }
            hc.dcbaa.write64(0, list.addr());
        }

        let op = hc.op;
        op.write32(CONFIG, op.read32(CONFIG) & !0xff | hc.max_slots as u32);
        write64(&op, DCBAAP, hc.dcbaa.addr());
        write64(&op, CRCR, hc.cmd.mem.addr() | TRB_CYCLE as u64);

        // One event ring segment table entry: (base, size in TRBs).
        let erst = Region::dma(1);
        erst.write64(0, hc.events.mem.addr());
        erst.write64(8, RING_TRBS as u64);
        hc.ir.write32(ERSTSZ, 1);
        write64(&hc.ir, ERDP, hc.events.mem.addr());
        write64(&hc.ir, ERSTBA, erst.addr());

        dsb();
        op.write32(USBCMD, USBCMD_RUN);
        if !wait_for(TIMEOUT_MS, || op.read32(USBSTS) & USBSTS_HALTED == 0) {
            return Err("controller did not start");
        }
        Ok(hc)
    }

    /// Walks the extended capability list and, if the firmware still owns
    /// the controller through the legacy support capability, asks for it.
    fn take_ownership(&self) -> Result<(), &'static str> {
        let cap = self.cap;
        let mut off = (cap.read32(HCCPARAMS1) >> 16) as usize * 4;
        while off != 0 {
            let v = cap.read32(off);
            if v & 0xff == XCAP_LEGACY && v & LEGACY_BIOS_OWNED != 0 {
                cap.write32(off, v | LEGACY_OS_OWNED);
                if !wait_for(TIMEOUT_MS, || cap.read32(off) & LEGACY_BIOS_OWNED == 0) {
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
        op.write32(USBCMD, op.read32(USBCMD) & !USBCMD_RUN);
        if !wait_for(TIMEOUT_MS, || op.read32(USBSTS) & USBSTS_HALTED != 0) {
            return Err("controller did not halt");
        }
        op.write32(USBCMD, USBCMD_RESET);
        if !wait_for(TIMEOUT_MS, || {
            op.read32(USBCMD) & USBCMD_RESET == 0 && op.read32(USBSTS) & USBSTS_NOT_READY == 0
        }) {
            return Err("controller did not come out of reset");
        }
        Ok(())
    }

    fn portsc(port: u8) -> usize {
        PORTSC + (port as usize - 1) * PORT_STRIDE
    }

    /// Numbers of the root hub ports with something plugged in.
    pub fn connected_ports(&self) -> impl Iterator<Item = u8> + '_ {
        (1..=self.max_ports).filter(|&n| self.op.read32(Self::portsc(n)) & PORTSC_CONNECTED != 0)
    }

    /// Resets a port unless it is already enabled (USB 3 ports enable
    /// themselves), which also makes its speed known.
    pub fn enable_port(&self, number: u8) -> Result<Port, &'static str> {
        let (op, reg) = (self.op, Self::portsc(number));
        let sc = op.read32(reg);
        if sc & PORTSC_ENABLED == 0 {
            op.write32(reg, sc & PORTSC_KEEP | PORTSC_RESET);
            if !wait_for(TIMEOUT_MS, || op.read32(reg) & PORTSC_RESET_CHANGE != 0) {
                return Err("port reset timed out");
            }
            // USB 2.0 gives a device 10 ms to recover after reset.
            wait_for(10, || false);
        }
        op.write32(reg, op.read32(reg) & PORTSC_KEEP | PORTSC_CHANGES);
        let sc = op.read32(reg);
        if sc & PORTSC_ENABLED == 0 {
            return Err("port did not enable");
        }
        Ok(Port { number, speed: ((sc >> 10) & 0xf) as u8 })
    }

    fn ring_doorbell(&self, slot: u8, target: u32) {
        // TRBs written to RAM before the MMIO write that announces them.
        dsb();
        self.db.write32(slot as usize * 4, target);
    }

    /// Polls the event ring until `want` matches an event; other events
    /// (port status changes) are dropped.
    fn wait_event(&mut self, want: impl Fn(&Trb) -> bool) -> Result<Trb, &'static str> {
        let (events, ir) = (&mut self.events, self.ir);
        let mut found = None;
        wait_for(TIMEOUT_MS, || {
            while let Some(e) = events.pop(&ir) {
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

    /// Context `index` of an input context; its dwords are at offsets 0, 4, ...
    fn context(&self, input: &Region, index: usize) -> Region {
        input.sub(index * self.ctx_size, self.ctx_size)
    }

    /// Fills the device's input context: control (`add` flags), slot (with
    /// contexts up to index `entries` in use) and endpoint 0.
    fn prepare_input(&self, dev: &Device, add: u32, entries: u32) {
        dev.input.zero();
        let control = self.context(&dev.input, 0);
        control.write32(4, add);
        let slot = self.context(&dev.input, 1);
        slot.write32(0, (dev.speed as u32) << 20 | entries << 27);
        slot.write32(4, (dev.port as u32) << 16);
        let ep0 = self.context(&dev.input, 2);
        ep0.write32(4, EP_ERROR_COUNT | EP_CONTROL << 3 | (dev.max_packet0 as u32) << 16);
        let ring = dev.ep0.mem.addr() | TRB_CYCLE as u64;
        ep0.write32(8, ring as u32);
        ep0.write32(12, (ring >> 32) as u32);
        ep0.write32(16, 8); // average TRB length: setup packets
    }

    fn input_command(&mut self, dev: &Device, ty: u32) -> Result<u8, &'static str> {
        let input = dev.input.addr();
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
            input: Region::dma(1),
            ep0: Ring::new(),
            // Until the descriptor says otherwise: the minimum per speed.
            max_packet0: match port.speed {
                3 => 64,
                4.. => 512,
                _ => 8,
            },
            buf: Region::dma(1),
        };
        self.dcbaa.write64(slot as usize * 8, Region::dma(1).addr());
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
            let buf = dev.buf.addr();
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
        let ctx = self.context(&dev.input, 1 + dci as usize);
        ctx.write32(0, period << 16);
        ctx.write32(4, EP_ERROR_COUNT | EP_INTERRUPT_IN << 3 | (max_packet as u32) << 16);
        let tr = ring.mem.addr() | TRB_CYCLE as u64;
        ctx.write32(8, tr as u32);
        ctx.write32(12, (tr >> 32) as u32);
        // Average TRB length and max payload per service interval.
        ctx.write32(16, max_packet as u32 | (max_packet as u32) << 16);
        self.input_command(dev, TRB_CONFIGURE_ENDPOINT)?;
        // Interrupt endpoints carry at most 1024 bytes per packet.
        let buf = Region::dma((IN_FLIGHT * max_packet as usize).div_ceil(mm::PAGE_SIZE));
        Ok(InterruptIn { slot: dev.slot, dci, ring, buf, len: max_packet as u32, done: 0 })
    }

    /// Queues one transfer into buffer `index` of the endpoint.
    pub fn submit(&mut self, ep: &mut InterruptIn, index: usize) {
        let buf = ep.buf.addr() + (index * ep.len as usize) as u64;
        let flags = TRB_NORMAL << 10 | TRB_IOC | TRB_SHORT_OK;
        ep.ring.push([buf as u32, (buf >> 32) as u32, ep.len, flags]);
        self.ring_doorbell(ep.slot, ep.dci);
    }

    /// Checks for the next completed transfer: which buffer, and the number
    /// of bytes received or an error. `None` while none has completed.
    pub fn poll(&mut self, ep: &mut InterruptIn) -> Option<(usize, Result<usize, &'static str>)> {
        while let Some(e) = self.events.pop(&self.ir) {
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
