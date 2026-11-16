# Yakimo test-machine installation: stage 3 only

This runbook describes one installation method only: extract the stage 3
archive from another Linux system, chroot into it, install the kernel, and add
a boot entry. The older source-bootstrap and container-bootstrap routes are
intentionally not part of this procedure.

## 1. Locate the stage 3 release

The stage 3 archive and its SHA-256 file are published as release assets at:

```text
https://github.com/asizeland/yakimo-stage3/releases/tag/Yakimo
```

The direct download commands below require this release and its assets to be
publicly accessible.

The archive contains 67 installed packages, the static `pkg` executable, the
package store and database, the installed recipe tree, a base userspace, a
compiler toolchain, Git, networking, SysVinit, and OpenRC. It does not contain a
kernel or bootloader. Those are installed or configured below.

The archive passed extraction, package-installation, and QEMU boot tests. A
physical-machine installation has not yet been verified. This is a published
test artifact, not a production installation release.

## 2. Prepare another Linux system and download stage 3

Boot either the machine's existing Linux installation or another
distribution's live USB. A Yakimo ISO is not required. The environment must
provide network access, curl, and standard disk, mount, tar, xz, and chroot
tools.

Back up data you cannot replace. Partitioning and formatting can destroy data.
Keep the existing operating system and its boot entry available as a recovery
route.

Start one root shell and use it for all host-side commands:

```sh
sudo -i
```

Create a directory for the downloaded release files:

```sh
mkdir -p /tmp/yakimo-stage3
cd /tmp/yakimo-stage3
```

Download the archive:

```sh
curl -fL --retry 3 -O https://github.com/asizeland/yakimo-stage3/releases/download/Yakimo/yakimo-stage3-0.1.0-x86_64.tar.xz
```

Download its SHA-256 file:

```sh
curl -fL --retry 3 -O https://github.com/asizeland/yakimo-stage3/releases/download/Yakimo/yakimo-stage3-0.1.0-x86_64.tar.xz.sha256
```

Set the archive and target mount paths:

```sh
export STAGE3=/tmp/yakimo-stage3/yakimo-stage3-0.1.0-x86_64.tar.xz
export PKG_ROOT=/mnt/yakimo
```

Verify that both release files exist:

```sh
test -s "$STAGE3"
test -s "$STAGE3.sha256"
```

Verify the archive before writing anything to the target:

```sh
(cd "$(dirname "$STAGE3")" && sha256sum -c "$(basename "$STAGE3").sha256")
```

Do not continue unless the checksum command reports `OK`.

## 3. Identify the disk and existing boot layout

Inspect the machine before selecting a partition:

```sh
lsblk -o NAME,PATH,SIZE,FSTYPE,FSVER,LABEL,UUID,MOUNTPOINTS,MODEL
findmnt /
findmnt /boot/efi
ls -l /dev/disk/by-id/
```

For a UEFI installation, confirm that the live or installed system booted in
UEFI mode:

```sh
ls -d /sys/firmware/efi
```

Distinguish the **whole disk** from its **partitions**. For example,
`/dev/nvme0n1` is a disk and `/dev/nvme0n1p3` is a partition. The partition
editor operates on the whole disk; formatting and mounting operate on the new
partition.

Do not format the existing root, EFI, home, recovery, or swap partition. Reuse
the existing EFI system partition and bootloader. Device names can change
between an installed system and a live USB, so use the names reported now.

## 4. Create the Yakimo root partition

Use an existing disposable partition or unallocated space on the intended disk.
A 100 GiB or larger ext4 partition is a practical test layout. A separate
`/boot`, `/home`, or swap partition is not required.

If a suitable disposable partition already exists, skip ahead to the
`export TARGET_ROOT=...` command below. Otherwise, open `fdisk` on the **whole
disk**:

```sh
fdisk /dev/disk/by-id/CHANGE_ME_WHOLE_DISK
```

Inside `fdisk`:

1. Enter `p`. Check the disk model, size, partitions, and free space. Enter `q`
   to leave without saving if anything is wrong.
2. Enter `n` to create a partition in the intended unallocated space.
3. Accept the proposed partition number and first sector only if they refer to
   that free area. For the last sector, enter a size such as `+100G`, or accept
   the default to use the remaining free space.
4. If asked for a partition type, choose `Linux filesystem`. Do not choose EFI,
   swap, or RAID.
5. Enter `p` again. Confirm that every existing partition remains and that only
   the intended partition will be added.
6. Enter `w` to save the partition table. This is the operation that writes the
   changes.

Return to the shell and ask the kernel to reread the table:

```sh
partprobe /dev/disk/by-id/CHANGE_ME_WHOLE_DISK
```

Identify the new partition:

```sh
lsblk -o NAME,PATH,SIZE,FSTYPE,LABEL,UUID,MOUNTPOINTS,MODEL
ls -l /dev/disk/by-id/
```

If the kernel cannot reread the table because another partition on that disk is
in use, reboot and identify the partition again. Do not continue with stale
device information.

Set `TARGET_ROOT` to the **new partition**, not the whole disk:

```sh
export TARGET_ROOT=/dev/disk/by-id/CHANGE_ME_ROOT_PARTITION
```

Check the selected partition:

```sh
lsblk -f "$TARGET_ROOT"
findmnt --source "$TARGET_ROOT"
```

`findmnt --source` should print nothing. Stop if the target is mounted or used
as swap. Unmount or disable it only after confirming that it is the disposable
Yakimo partition.

## 5. Format and mount the target

The next command permanently erases `TARGET_ROOT`. Recheck the previous output
before running it:

```sh
mkfs.ext4 -L yakimo "$TARGET_ROOT"
```

Create the mount point:

```sh
mkdir -p "$PKG_ROOT"
```

Mount only the new Yakimo partition:

```sh
mount "$TARGET_ROOT" "$PKG_ROOT"
```

Verify the mount and record its filesystem UUID:

```sh
findmnt "$PKG_ROOT"
export ROOT_UUID=$(blkid -s UUID -o value "$TARGET_ROOT")
printf 'ROOT_UUID=%s\n' "$ROOT_UUID"
test -n "$ROOT_UUID"
```

Do not continue unless `findmnt` shows the intended partition at `/mnt/yakimo`.

## 6. Extract the stage 3 archive

Extract the verified archive with numeric ownership preserved:

```sh
tar --numeric-owner -xJf "$STAGE3" -C "$PKG_ROOT"
```

Check that the package manager and base system are present:

```sh
test -x "$PKG_ROOT/usr/bin/pkg"
test -x "$PKG_ROOT/usr/bin/bash"
test -d "$PKG_ROOT/var/lib/pkg/recipes"
```

No host-side Cargo build, package build, `PKG` variable, or external ports
checkout is needed for this method.

## 7. Mount the filesystems required by the chroot

Mount proc:

```sh
mount -t proc proc "$PKG_ROOT/proc"
```

Bind sys recursively and prevent mount events from propagating back:

```sh
mount --rbind /sys "$PKG_ROOT/sys"
mount --make-rslave "$PKG_ROOT/sys"
```

Bind dev recursively, including devpts, and prevent propagation back:

```sh
mount --rbind /dev "$PKG_ROOT/dev"
mount --make-rslave "$PKG_ROOT/dev"
```

Provide a temporary runtime filesystem:

```sh
mount -t tmpfs tmpfs "$PKG_ROOT/run"
```

Copy the host's DNS configuration so source downloads work in the chroot:

```sh
cp -L /etc/resolv.conf "$PKG_ROOT/etc/resolv.conf"
```

Yakimo's Bubblewrap sandbox requires user-namespace support from the **host
kernel**. Check the generic namespace limit while still in the host root shell:

```sh
sysctl -n user.max_user_namespaces
```

If this prints `0`, enable a reasonable limit for the current host session:

```sh
sysctl -w user.max_user_namespaces=1024
```

Gentoo kernels normally do not have Debian's
`kernel.unprivileged_userns_clone` setting; an error about that missing setting
is not a problem on Gentoo. If `/proc/config.gz` exists, check that the host
kernel has user namespaces compiled in:

```sh
zgrep CONFIG_USER_NS /proc/config.gz
```

The required result is `CONFIG_USER_NS=y`. If the configuration is unavailable,
use the Bubblewrap check below instead. These namespace settings affect the
host kernel, not only the Yakimo target.

## 8. Enter the target and install its kernel

Enter the chroot without retaining the host-side root override:

```sh
env -u PKG_ROOT chroot "$PKG_ROOT" /usr/bin/bash --login
```

The commands in this section run **inside the Yakimo chroot**. Set the command
search path:

```sh
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
```

Check the extracted package database and installed files:

```sh
pkg db check
pkg check
```

Verify that Bubblewrap can create a user namespace before starting a build:

```sh
bwrap --unshare-user --ro-bind / / /bin/true
```

If this fails with a namespace-permission error, stop and correct the host
kernel's user-namespace support or limit. Do not continue to the package build
and do not disable Yakimo's sandbox.

The archive includes a flat local recipe tree at `/var/lib/pkg/recipes`, but the
CLI must be told to use it for local source builds. Install the kernel and
wireless regulatory database with that tree selected explicitly:

```sh
pkg install -y --recipe-tree /var/lib/pkg/recipes linux wireless-regdb
```

Register the kernel modules:

```sh
depmod -a 6.19.14-yakimo
```

Verify the kernel files:

```sh
test -s /boot/vmlinuz-6.19.14-yakimo
test -s /boot/config-6.19.14-yakimo
```

## 9. Configure the installed system

Remain inside the chroot. Write the real root filesystem UUID recorded on the
host to `/etc/fstab`:

```sh
printf 'UUID=%s  /  ext4  rw,relatime  0  1\n' "$ROOT_UUID" > /etc/fstab
```

Check the resulting file:

```sh
cat /etc/fstab
```

Set the test hostname:

```sh
printf 'yakimo-test\n' > /etc/hostname
```

Set the matching local host entries:

```sh
cat > /etc/hosts <<'EOF'
127.0.0.1 localhost yakimo-test
::1 localhost yakimo-test
EOF
```

The supplied configuration starts SysVinit, OpenRC, eudev, D-Bus, and
NetworkManager. It also starts passwordless root consoles on tty1 and ttyS0.
**Those consoles grant root without authentication. Use them only for isolated
testing, and install a real login stack before production or multi-user use.**

The kernel boots the tested ext4 setup directly. Do not add an initrd entry
unless an initramfs has actually been built. Physical storage, early microcode,
and other hardware may require kernel or boot-configuration changes.

Leave the chroot:

```sh
exit
```

## 10. Add a boot entry from the host

The commands in this section run in the **host root shell** again. Keep the
existing host bootloader and default entry. Do not install a replacement
bootloader for this test.

Check the kernel that the host bootloader must load:

```sh
test -s "$PKG_ROOT/boot/vmlinuz-6.19.14-yakimo"
printf 'Root filesystem UUID: %s\n' "$ROOT_UUID"
```

For an existing GRUB installation, add the following entry to the host's
`/etc/grub.d/40_custom`, preserving that file's existing contents. Replace both
`CHANGE_ME_ROOT_UUID` occurrences with the value printed above:

```grub
menuentry 'Yakimo test - 6.19.14' {
    search --no-floppy --fs-uuid --set=root CHANGE_ME_ROOT_UUID
    linux /boot/vmlinuz-6.19.14-yakimo root=UUID=CHANGE_ME_ROOT_UUID rootfstype=ext4 rw
}
```

There is deliberately no `initrd` line. Regenerate the host GRUB configuration:

```sh
grub-mkconfig -o /boot/grub/grub.cfg
```

If the host uses another bootloader, add the equivalent entry using that
bootloader's normal configuration mechanism instead. Do not overwrite the
host's existing Yakimo-independent recovery entry.

## 11. Unmount the target

Ensure that no process is still using the target, then synchronize writes:

```sh
sync
```

Unmount proc:

```sh
umount "$PKG_ROOT/proc"
```

Unmount the temporary runtime filesystem:

```sh
umount "$PKG_ROOT/run"
```

Unmount sys and its recursive binds:

```sh
umount -R "$PKG_ROOT/sys"
```

Unmount dev and its recursive binds:

```sh
umount -R "$PKG_ROOT/dev"
```

Finally unmount the Yakimo root:

```sh
umount "$PKG_ROOT"
```

If an unmount fails, identify and stop the process using the target. Do not hide
the problem with a lazy unmount.

Reboot and select **Yakimo test - 6.19.14** from the boot menu. Keep the existing
operating system entry available as the recovery route.

## 12. Check the first boot

At the test-only root console, confirm the running kernel and root filesystem:

```sh
cat /proc/version
findmnt /
```

Check services:

```sh
rc-status
```

Check the installed package state:

```sh
pkg db check
pkg check
```

Check networking:

```sh
ip -brief address
ip route
getent hosts github.com
curl -fsSI https://github.com >/dev/null
```

Exercise the package manager using its installed recipe tree explicitly:

```sh
pkg install -y --recipe-tree /var/lib/pkg/recipes tty-solitaire
test -x /usr/bin/ttysolitaire
pkg history
pkg undo -y
test ! -e /usr/bin/ttysolitaire
pkg db check
pkg check
```

Record any failure with the exact command, relevant build log, `dmesg` excerpt,
and recipe-tree commit. Do not describe the physical installation as verified
until the base boot, package checks, networking, installation, and undo checks
all pass on the real machine.
