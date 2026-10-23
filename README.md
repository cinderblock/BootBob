# BootBob

A network boot server with a web UI. Machines that netboot from it land on a themed
fallback menu, show up in the web UI straight away, and can be pointed at something else
from the browser while they wait. BootBob is BenchBob's sibling
([DeskyPi](https://github.com/cinderblock/DeskyPi)): same stack, same look.

**Status: early (Phase 1 of the plan).** PCs (legacy BIOS and UEFI) netboot into BootBob's
iPXE and its fallback menu, and can be assigned images from the web UI. Raspberry Pi
native boot, network roots, installers, backup/restore and the controller image come in
later phases. The design and plan live in DeskyPi's
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
interface = "eno1"         # the DHCP listener; leave out to turn it off
```

Every setting is optional; see [`config.rs`](backend/crates/bootbob/src/config.rs).
Without a config, BootBob serves HTTP on `:8067` and TFTP on `:69` from `./data` and `./ipxe`.

## Layout

| Path | What |
| --- | --- |
| `backend/` | Rust workspace: `bootbob` (the server), `bootbob-tftp`, `bootbob-dhcp`, `bootbob-ipxe` |
| `web/` | The web UI: Preact (via preact/compat), Vite, TypeScript, Bun |
| `ipxe/` | iPXE build: pinned commit, embedded script, build options |
| `e2e/` | End-to-end test: SeaBIOS and OVMF VMs netboot from BootBob in QEMU |

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
| `GET /api/images`, `PUT/DELETE /api/images/{id}` | The catalogue (kernel, chain or script images) |
| `GET/PUT /api/settings` | Default image, menu poll interval, theme |
| `GET /api/events` | The log |
| `GET /api/stream` | Server-sent events: what changed |
| `GET /boot/ipxe?mac=…` | What iPXE asks; returns a script |

## License

MIT
