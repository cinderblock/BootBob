# Issues

## After a server crash, writes stall about 30 s even though the client reconnects at once

A request in flight when the connection dies is resent only when it times out
(`REQUEST_TIMEOUT` in `backend/crates/nbd-client/src/main.rs`):
`block nbd0: Connection timed out, retrying (1/1 alive)` about 30 s after the drop. Nothing
is lost and the root stays read-write. A normal restart no longer does this, because the
server answers what it was sent before it exits (`Live::drain`); a killed or crashed
server still does.

A shorter timeout isn't free: it would make the kernel reconnect during any slow request,
which then waits for the old connection to finish, and the timeout is baked into each image's
initramfs.

## Stock images can't run their root over NBD

The catalogue fetches, verifies and serves a stock Raspberry Pi OS image, and TFTP serves its
boot files, but its initramfs has no NBD client, so the boot stops at the root filesystem.
Cloud images have the same problem, and keep their kernels on the root filesystem rather
than a FAT partition (`PiBoot`/`DiskBoot` read kernels from the FAT partition). So today only
images built with `tools/make-pi-golden.sh` or `tools/make-pc-golden.sh`, or kernels and
installers that need no disk, boot end to end from the catalogue. It matters because "Pi OS
from the catalogue, on a Pi" looks like it should work, and is what people will try first.

## The catalogue can't take qcow2, zip or tar images

`sparse::not_a_disk` refuses them with a clear message. That rules out Ubuntu's cloud images
(`.img` that is really qcow2) and about half of the third-party images in Raspberry Pi's list
(`.zip`; `catalogue::parse_pi_os` leaves them out). Debian's `.raw` and Pi OS's `.img.xz` work.

## A Pi that reboots quickly ignores a new assignment

`PI_SESSION_GAP_MS` (60 s, `backend/crates/bootbob/src/model.rs`) decides that TFTP requests
belong to one boot, so `Tftp::pi_session` keeps serving the same image. A Pi that reboots
within 60 s of its last request looks like the same boot. Seen on desky, 2026-11-01 12:15-12:16
PM PST (20:15-20:16Z): assigned back to `desky`, rebooted 12 s after its last TFTP read, and
booted the one-shot test image a second time; the event log shows `start4.elf` served from the
old image, with no new `Booting` line. A reboot after 60 s worked. It matters because a one-shot
boot runs twice, and a machine told to leave an image can stay on it. The DHCP listener's
`Started a network boot` event fired at the time, so a new boot was visible.
