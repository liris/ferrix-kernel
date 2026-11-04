//! USB devices on the xHCI root hub: enumerate every connected port, say
//! what is plugged in, and drive the first boot protocol keyboard.
//!
//! The keyboard is switched to the HID boot protocol, whose fixed 8-byte
//! report needs no report descriptor parsing. A `kbd` task then keeps
//! `xhci::IN_FLIGHT` interrupt IN transfers queued, polls for completions
//! once per tick and feeds each report to `keyboard`. Hubs are not supported yet, so only
//! devices on root ports are seen.

use crate::global::Global;
use crate::keyboard::{self, BootKeyboard};
use crate::xhci::{self, DESC_DEVICE, Device, InterruptIn, REQ_GET_DESCRIPTOR, REQ_IN, Xhci};
use crate::{println, task};
use alloc::vec::Vec;

const REQ_SET_CONFIGURATION: u8 = 9;
const REQ_HID_SET_PROTOCOL: u8 = 0x0b;
/// bmRequestType: host to device, class request, to an interface.
const REQ_CLASS_INTERFACE: u8 = 0x21;
const HID_BOOT_PROTOCOL: u16 = 0;

const DESC_CONFIG: u16 = 2;
const DESC_INTERFACE: u8 = 4;
const DESC_ENDPOINT: u8 = 5;
const ENDPOINT_IN: u8 = 0x80;
const ENDPOINT_INTERRUPT: u8 = 3;

const CLASS_HID: u8 = 3;
const CLASS_MASS_STORAGE: u8 = 8;
const CLASS_HUB: u8 = 9;
const CLASS_WIRELESS: u8 = 0xe0;
const HID_SUBCLASS_BOOT: u8 = 1;
const HID_KEYBOARD: u8 = 1;
const HID_MOUSE: u8 = 2;

struct Endpoint {
    address: u8,
    max_packet: u16,
    interval: u8,
}

struct Interface {
    number: u8,
    class: u8,
    subclass: u8,
    protocol: u8,
    /// Its first interrupt IN endpoint.
    interrupt_in: Option<Endpoint>,
}

impl Interface {
    fn is_boot_keyboard(&self) -> bool {
        self.class == CLASS_HID
            && self.subclass == HID_SUBCLASS_BOOT
            && self.protocol == HID_KEYBOARD
    }
}

/// What a device told us about itself.
struct Probed {
    dev: Device,
    vendor: u16,
    product: u16,
    class: u8,
    config: u8,
    interfaces: Vec<Interface>,
}

impl Probed {
    fn kind(&self) -> &'static str {
        if self.class == CLASS_HUB {
            return "hub (not supported)";
        }
        let Some(i) = self.interfaces.first() else {
            return "no interfaces";
        };
        match (i.class, i.protocol) {
            _ if self.interfaces.iter().any(Interface::is_boot_keyboard) => "keyboard",
            (CLASS_HID, HID_MOUSE) => "mouse",
            (CLASS_HID, _) => "HID",
            (CLASS_MASS_STORAGE, _) => "storage",
            (CLASS_WIRELESS, _) => "wireless",
            _ => "other",
        }
    }
}

fn u16_at(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

/// Splits a configuration descriptor (with everything that follows it) into
/// its interfaces.
fn parse_config(bytes: &[u8]) -> Vec<Interface> {
    let mut interfaces: Vec<Interface> = Vec::new();
    let mut off = 0;
    while off + 2 <= bytes.len() {
        let len = bytes[off] as usize;
        if len < 2 || off + len > bytes.len() {
            break;
        }
        let d = &bytes[off..off + len];
        match d[1] {
            DESC_INTERFACE if len >= 9 => interfaces.push(Interface {
                number: d[2],
                class: d[5],
                subclass: d[6],
                protocol: d[7],
                interrupt_in: None,
            }),
            DESC_ENDPOINT if len >= 7 => {
                if let Some(i) = interfaces.last_mut()
                    && i.interrupt_in.is_none()
                    && d[2] & ENDPOINT_IN != 0
                    && d[3] & 0x3 == ENDPOINT_INTERRUPT
                {
                    i.interrupt_in = Some(Endpoint {
                        address: d[2],
                        max_packet: u16_at(d, 4) & 0x7ff,
                        interval: d[6],
                    });
                }
            }
            _ => {}
        }
        off += len;
    }
    interfaces
}

fn probe(hc: &mut Xhci, number: u8) -> Result<Probed, &'static str> {
    let port = hc.enable_port(number)?;
    let mut dev = hc.address_device(&port)?;
    hc.control(&mut dev, REQ_IN, REQ_GET_DESCRIPTOR, DESC_DEVICE << 8, 0, 18)?;
    let d = dev.data(18);
    let (class, vendor, product) = (d[4], u16_at(d, 8), u16_at(d, 10));

    // The header first, for the total length of everything that follows.
    hc.control(&mut dev, REQ_IN, REQ_GET_DESCRIPTOR, DESC_CONFIG << 8, 0, 9)?;
    let total = u16_at(dev.data(9), 2);
    let config = dev.data(9)[5];
    hc.control(&mut dev, REQ_IN, REQ_GET_DESCRIPTOR, DESC_CONFIG << 8, 0, total)?;
    let interfaces = parse_config(dev.data(total as usize));
    Ok(Probed { dev, vendor, product, class, config, interfaces })
}

fn setup_keyboard(hc: &mut Xhci, p: &mut Probed) -> Result<InterruptIn, &'static str> {
    let i = p.interfaces.iter().find(|i| i.is_boot_keyboard()).ok_or("no keyboard interface")?;
    let ep = i.interrupt_in.as_ref().ok_or("keyboard has no interrupt IN endpoint")?;
    let (number, address, max_packet, interval) =
        (i.number, ep.address, ep.max_packet, ep.interval);
    hc.control(&mut p.dev, 0, REQ_SET_CONFIGURATION, p.config as u16, 0, 0)?;
    hc.control(
        &mut p.dev,
        REQ_CLASS_INTERFACE,
        REQ_HID_SET_PROTOCOL,
        HID_BOOT_PROTOCOL,
        number as u16,
        0,
    )?;
    hc.configure_interrupt_in(&p.dev, address, max_packet, interval)
}

/// Hands the controller and keyboard endpoint to the `kbd` task.
static HANDOVER: Global<(Xhci, InterruptIn)> = Global::new("keyboard handover");

/// Lists the devices on the root hub and starts the keyboard task if a
/// keyboard is among them. Returns whether one was.
pub fn init(mut hc: Xhci) -> bool {
    let ports: Vec<u8> = hc.connected_ports().collect();
    if ports.is_empty() {
        println!("[usb ] nothing connected");
    }
    let mut keyboard = None;
    for n in ports {
        let mut p = match probe(&mut hc, n) {
            Ok(p) => p,
            Err(e) => {
                println!("[usb ] port {}: {}", n, e);
                continue;
            }
        };
        println!(
            "[usb ] port {} {} {:04x}:{:04x} {}",
            n,
            xhci::speed_name(p.dev.speed),
            p.vendor,
            p.product,
            p.kind()
        );
        if keyboard.is_none() && p.kind() == "keyboard" {
            match setup_keyboard(&mut hc, &mut p) {
                Ok(ep) => keyboard = Some(ep),
                Err(e) => println!("[usb ] keyboard: {}", e),
            }
        }
    }
    let Some(ep) = keyboard else {
        return false;
    };
    HANDOVER.set((hc, ep));
    task::spawn("kbd", keyboard_task);
    true
}

/// Keeps report requests queued and passes every report on, in order. A
/// failed transfer is counted and simply requested again.
fn keyboard_task() {
    let Some((mut hc, mut ep)) = HANDOVER.take() else {
        return;
    };
    let mut kbd = BootKeyboard::default();
    for i in 0..xhci::IN_FLIGHT {
        hc.submit(&mut ep, i);
    }
    loop {
        match hc.poll(&mut ep) {
            None => task::sleep(1),
            Some((i, result)) => {
                match result {
                    // SAFETY: `poll` just returned buffer `i`, and it is resubmitted only
                    // below, after the report is read.
                    Ok(len) => kbd.report(unsafe { ep.data(i, len) }),
                    Err(_) => keyboard::count_error(),
                }
                hc.submit(&mut ep, i);
            }
        }
    }
}
