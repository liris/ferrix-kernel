//! Device drivers: firmware tables (ACPI), buses (PCIe, USB), the interrupt
//! controller and timer, and the framebuffer console.

pub mod acpi;
pub mod console;
pub mod gic;
pub mod irq;
pub mod pci;
pub mod usb;
