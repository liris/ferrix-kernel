NAME    := ferrix-kernel
EFI     := target/aarch64-unknown-uefi/release/$(NAME).efi
OUT     := build
ISO     := $(OUT)/$(NAME).iso
ESP_IMG := $(OUT)/iso_root/efiboot.img
EDK2    := $(shell brew --prefix qemu)/share/qemu/edk2-aarch64-code.fd

QEMU := qemu-system-aarch64 -M virt,gic-version=3 -cpu cortex-a72 -m 512M \
	-bios $(EDK2) -device ramfb \
	-device qemu-xhci -device usb-kbd \
	-device virtio-scsi-pci -drive if=none,id=cd,format=raw,media=cdrom,file=$(ISO) \
	-device scsi-cd,drive=cd

VMX_TEMPLATE := vmware/$(NAME).vmx
VM_DIR       := $(OUT)/$(NAME).vmwarevm
VMRUN        := /Applications/VMware\ Fusion.app/Contents/Public/vmrun

.PHONY: all build iso run run-headless vm vm-run clean

all: iso

# e.g. `make run FEATURES=fault-test`
FEATURES ?=

build:
	cargo build --release $(if $(FEATURES),--features $(FEATURES))

# FAT image holding EFI/BOOT/BOOTAA64.EFI, wrapped as an El Torito EFI boot entry.
iso: build
	rm -rf $(OUT)/iso_root && mkdir -p $(OUT)/iso_root
	dd if=/dev/zero of=$(ESP_IMG) bs=1k count=2880 status=none
	mformat -i $(ESP_IMG) -f 2880 ::
	mmd -i $(ESP_IMG) ::/EFI ::/EFI/BOOT
	mcopy -i $(ESP_IMG) $(EFI) ::/EFI/BOOT/BOOTAA64.EFI
	xorriso -as mkisofs -quiet -R -J -V FERRIX \
		-e efiboot.img -no-emul-boot -o $(ISO) $(OUT)/iso_root

run: iso
	$(QEMU) -serial stdio

# For automated checks: no window, monitor on a socket for screendump.
run-headless: iso
	$(QEMU) -display none -serial file:$(OUT)/serial.log \
		-monitor unix:$(OUT)/mon.sock,server,nowait

# VMware Fusion VM next to the ISO; the template refers to ../$(NAME).iso.
vm: iso $(VM_DIR)/template.vmx

# VMware rewrites the VM's .vmx on every boot, so its timestamp says nothing;
# a pristine copy of the template marks which version was installed.
$(VM_DIR)/template.vmx: $(VMX_TEMPLATE)
	mkdir -p $(VM_DIR)
	cp $(VMX_TEMPLATE) $(VM_DIR)/$(NAME).vmx
	cp $(VMX_TEMPLATE) $@

# Always cold-boots the freshly built ISO: a running VM is powered off first
# (so template changes to the .vmx take effect), and suspended state is
# discarded so it does not resume.
VMX := $(abspath $(VM_DIR)/$(NAME).vmx)

vm-run: iso
	if $(VMRUN) list | grep -qF "$(VMX)"; then \
		$(VMRUN) -T fusion stop "$(VMX)" hard; \
	fi
	$(MAKE) -s vm
	rm -f $(VM_DIR)/*.vmss $(VM_DIR)/*.vmem $(VM_DIR)/serial.log
	$(VMRUN) -T fusion start "$(VMX)" gui

clean:
	cargo clean
	rm -rf $(OUT)
