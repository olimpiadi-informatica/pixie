# Preparing correct kernel/ramdisk images for pixie

- Build a suitable kernel for your target machine(s). Name the obtained kernel
  vmlinuz.img and put it in this folder
- Build a suitable busybox that can run the programs used in the init script.
  Put it in the initrd/bin folder
- Build a static version of mke2fs and put it in the initrd/bin folder, naming
  it mkfs.ext4
- Compile the client and put the resulting binary in the initrd/bin folder,
  naming it pixie
- Execute the create_initrd.sh script in this folder
- Copy the vmlinuz.img and initrd.img files in this folder in the root of your
  tftp server

