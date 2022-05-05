# Reboot to 2nd boot option
EFI binary to reboot to the second boot option in the EFI boot order.

Build with:

```
cargo +nightly build --release --target x86_64-unknown-uefi
```

Run on arch in qemu:

```
uefi-run -b /usr/share/edk2-ovmf/x64/OVMF_CODE.fd --qemu /bin/qemu-system-x86_64 target/x86_64-unknown-uefi/release/uefi_app.efi
```
