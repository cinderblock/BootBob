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
## Golden images don't take the time zone from DHCP

BootBob reads option 101, but a machine on a golden image stays on UTC. The Pi image would need
`UseTimezone=yes` under `[DHCPv4]`, and networkd sets it through timedated, so it probably needs
`polkit` and `tzdata` too, which the image doesn't ask for by name. Untested. The PC image runs no
network manager at all: the initrd's `ip=dhcp` configures the interface. Rollouts don't depend on this:
the window is the host's. It matters only for local time on the machines.

## A new version of a golden image gives a machine a new identity

A machine moving to a new version comes up on a fresh root, so `bootbob-first-boot` makes new SSH
host keys and a new machine-id each time (the Pi image; the PC image shares one set across every
copy, which is its own problem). `known_hosts` complains after every rollout. Keeping a machine's
identity across versions needs somewhere for it to live outside the root.

## No way to export a disk read-only to a machine

`Export` has a `read_only` field, but `backend/crates/bootbob/src/nbd.rs` always sets it to
`false` (both the local and the store export paths), and nothing in the image model can ask
for it. A machine that only needs a pristine root, such as a CI runner that reboots after every
job and keeps its writes in a tmpfs overlay, can still write to its copy-on-write overlay as
root. That means a job that gets root can persist something into the next boot. It matters for
the dfu-programmer hardware CI Pi (the AVR Test HAT), which wants "nothing a job does
survives a reboot" enforced by the server, not only by the Pi's own mounts. Wanted: a per-image
(or per-assignment) "read-only" setting that exports the disk read-only and refuses writes.
TFTP boot-file reads are unaffected.

## The control API has no authentication

The web UI and API (`[control] http`) have no login: anything that can reach them can reassign
what every machine boots, upload or delete disks, and reset overlays. Since multi-interface
support (`plans/multi-interface.md`), they listen only where `[control] http` says, each listener
bound to the interface holding its address, so a boot VLAN can't reach them. On the LAN they're
still open to everything (Caddy's LAN-only check on `pxe.tsl` doesn't cover the bare
`10.255.0.5:8067`, which machines on the LAN use to boot). It matters because any machine on the
LAN, including a netbooted one running something untrusted, can change what others boot.

Also still true within one network segment: MACs and addresses can be faked. BootBob now
identifies a machine only from the interface it asked on (its DHCP listener and neighbour table),
checks what it claims against that, and admits machines per boot interface, so the damage stays
inside the segment. An untrusted machine belongs on a VLAN of its own. Routed machines (TFTP from
another subnet) can't be identified at all, by design: give them a boot interface on their own
network.
