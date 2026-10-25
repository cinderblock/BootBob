# BootBob

A network boot server with a web UI. Machines that netboot from it land on a themed
fallback menu, show up in the web UI straight away, and can be pointed at something else
from the browser while they wait. BootBob is BenchBob's sibling
([DeskyPi](https://github.com/cinderblock/DeskyPi)): same stack, same look.

**Status: early (Phase 2 of the plan).** PCs (legacy BIOS and UEFI) netboot into BootBob's
iPXE and its fallback menu, and can be assigned images from the web UI. Raspberry Pis
netboot natively from boot trees and disk images. Network roots, installers,
backup/restore and the controller image come in later phases. The design and plan live in DeskyPi's
[`plans/pxe-netboot.md`](https://github.com/cinderblock/DeskyPi/blob/master/plans/pxe-netboot.md).

## How it fits on a network

BootBob **doesn't run DHCP**. The network's own DHCP server (a UniFi gateway, say) tells
machines where to boot from, with two settings:

| DHCP setting | Value |
| --- | --- |
| Boot server (next-server / option 66) | BootBob's address, e.g. `10.255.0.5` |
| Boot file (option 67) | `bootbob` |

One file name works for every firmware: when a machine asks for `bootbob` over TFTP,
BootBob hands it the iPXE build for its firmware (BIOS, UEFI x86-64 or UEFI arm64). It
knows which by **listening** to the machine's DHCP request (a receive-only packet socket;
it never sends DHCP). The same listener lets the UI show machines that tried to netboot
but never got as far as BootBob.

From there, iPXE fetches a script from BootBob over HTTP: either the image the machine is
assigned, or the fallback menu.

## The fallback menu

- Check for an assignment now (the default, every 10 s until someone presses a key;
  left untouched for 2 minutes, it boots the local disk, so a PC that merely tries the
  network first still starts normally)
- Boot from local disk
- Images from the catalogue, grouped (netboot.xyz is built in)
- Reboot, Power off, iPXE shell

A machine sitting at the menu shows as **waiting at menu** in the web UI. Assign it an
image ("next boot only", or permanently) and it boots it at its next check-in, with nobody
at the keyboard. Choices made at the menu are reported back, so the UI always knows what
each machine did.

## Raspberry Pis

A Pi 4 or 5 netboots with its own bootloader, no iPXE: set `BOOT_ORDER` to include network
boot (`0xf2`: network only, retrying forever) and `TFTP_IP` to BootBob's address in its
EEPROM. `TFTP_IP` is what lets it find BootBob without any Pi-specific DHCP options.

The Pi then asks for its boot files over TFTP, under its serial number or MAC. BootBob
serves them from the **Pi image** assigned to it (or the Pi default in Settings):

- **Boot files** are a directory, or a disk image (`.img`), in the images directory
  (`<data_dir>/images`). A disk image's FAT boot partition is read in place (MBR, GPT, or
  a bare FAT filesystem), so a decompressed Pi OS image works as downloaded.
- **`cmdline.txt`** can be replaced and **`config.txt`** added to, per image, with
  `{serial}`, `{mac}`, `{ip}` and `{server}` filled in for each Pi.

### Network roots (NBD)

With `nbd = "<address>:10809"` in the config, BootBob also serves disk images as network
block devices, so a Pi can run with no SD card at all. Make one disk image hold the whole
system (a FAT boot partition and the root filesystem), and use it as the Pi image's boot
files too: TFTP reads the kernel out of it, and the initramfs mounts its root over NBD
with `nbd-client` (`root=/dev/nbd0p2 nbdroot=<server>,<disk>.img,nbd0 ip=dhcp`). Kernel
updates the Pi installs land in the same image, so the next boot uses them.

A disk image is exported only to the machine assigned the Pi image it belongs to (or the
one running it now). `POST /api/disks` creates an empty one to fill over NBD.

**One image, many Pis.** Mark a Pi image *copy-on-write* and each machine gets its own
overlay of the disk (`<data_dir>/overlays/<mac>/`): a sparse file holding only the 4 KiB
chunks that machine has written, and a map of which they are. Reads come from the overlay
where it has the chunk and from the image elsewhere; the image itself is never written, so
it stays the golden copy. TFTP reads a machine's boot files through its overlay too, so a
kernel update it installs is what it boots next time. The web UI shows how much each
machine has written and can reset it to the pristine image (not while it's running from
it). Replacing the image file with one of a different size invalidates the overlays made
over it; reset them.

Every file of one boot comes from the image it started with, so a "next boot only"
assignment lasts the whole boot. A Pi with nothing to boot gets nothing, keeps asking, and
shows up in the web UI as waiting.

## Running it

```sh
docker build -t bootbob .
docker run --network host --cap-add NET_RAW \
  -v bootbob:/var/lib/bootbob -v ./bootbob.toml:/etc/bootbob/bootbob.toml:ro bootbob
```

Then open `http://<server>:8067/`. A minimal `bootbob.toml`:

```toml
data_dir = "/var/lib/bootbob"
ipxe_dir = "/usr/share/bootbob/ipxe"
web_dir = "/usr/share/bootbob/web"
http = "10.255.0.5:8067"   # listen on the LAN address only
tftp = "10.255.0.5:69"
nbd = "10.255.0.5:10809"   # network roots; leave out to turn it off
interface = "eno1"         # the DHCP listener; leave out to turn it off
```

Every setting is optional; see [`config.rs`](backend/crates/bootbob/src/config.rs).
Without a config, BootBob serves HTTP on `:8067` and TFTP on `:69` from `./data` and `./ipxe`.

## Layout

| Path | What |
| --- | --- |
| `backend/` | Rust workspace: `bootbob` (the server), `bootbob-tftp`, `bootbob-dhcp`, `bootbob-ipxe`, `bootbob-pi`, `bootbob-cow`, `bootbob-nbd` |
| `web/` | The web UI: Preact (via preact/compat), Vite, TypeScript, Bun |
| `ipxe/` | iPXE build: pinned commit, embedded script, build options |
| `e2e/` | End-to-end test: SeaBIOS and OVMF VMs netboot from BootBob in QEMU |
| `tools/` | `pi-clone-to-nbd.sh`: clone a running Pi onto a BootBob NBD disk, to run it diskless |

## Development

```sh
cd backend && cargo test --workspace && cargo clippy --workspace --all-targets
cd web && bun install && bun run typecheck && bun run test && bun run dev
```

`BOOTBOB_PROXY=http://<server>:8067 bun run dev` points the dev UI at a running server.

iPXE builds need Linux: `ipxe/build.sh <out-dir>` (needs `build-essential liblzma-dev`,
and `gcc-aarch64-linux-gnu` for arm64). CI builds them and runs the QEMU test
(`sudo e2e/run.sh <bootbob> <ipxe-dir>`) on every push. The e2e test runs dnsmasq as a
stand-in for the network's DHCP server, on a private bridge with no uplink, so it's only
for throwaway VMs.

## API

| Method and path | What |
| --- | --- |
| `GET /api/status` | Version, TFTP/listener state, iPXE builds found |
| `GET /api/devices`, `GET /api/devices/{mac}` | Machines, with their state and what they'd boot next |
| `PATCH /api/devices/{mac}` | `{"name": …, "notes": …}` |
| `PUT /api/devices/{mac}/assignment` | `{"image": "debian", "once": true}` |
| `DELETE /api/devices/{mac}/assignment` | Back to the default |
| `GET /api/images`, `PUT/DELETE /api/images/{id}` | The catalogue: an iPXE `action` (kernel, chain or script), a Raspberry Pi boot tree (`pi`), or both |
| `GET /api/devices/{mac}/overlays` | A machine's copy-on-write overlays: image, bytes written, in use |
| `DELETE /api/devices/{mac}/overlays/{image}` | Reset one: back on the pristine image at the next boot (409 while in use) |
| `POST /api/disks` | `{"name": "desky.img", "size": <bytes>}`: an empty disk image to fill over NBD |
| `GET /api/pi/sources` | Directories and disk images in the images directory, with the Pi boot files each has |
| `GET/PUT /api/settings` | Default image (PCs and Pis), menu poll interval, theme |
| `GET /api/events` | The log |
| `GET /api/stream` | Server-sent events: what changed |
| `GET /boot/ipxe?mac=…` | What iPXE asks; returns a script |

## License

MIT
