# Multi-interface and VLAN-aware BootBob

Branch `multi-interface` (worktree `C:\Users\camer\.t3\worktrees\BootBob\multi-interface`).

## Goal

BootBob serves boots on several network interfaces at once (`eno1`, plus tagged
subinterfaces like `eno1.5`). Each interface has its own policy: which services it
offers and which machines it may serve. BootBob knows which VLAN each interface and
machine is on, and the UI can show, say, "VLAN 5 hosts".

BootBob does **not** configure host networking. Interfaces and VLAN subinterfaces come
from the host (netplan, systemd-networkd, ops). BootBob discovers and uses them.

Why now: the dfu-programmer hardware-CI Pi runs untrusted jobs and is going onto its own
VLAN (plan: `C:\Users\camer\.t3\scratch\2026-11-10-goal-get-dfu-programmer-s-b8992507\plans\dfu-programmer-hardware-ci.md`).
On the flat LAN it could drive BootBob's unauthenticated API or claim another machine's
MAC (ISSUES.md, "Anything on the LAN can drive BootBob…", `0abf3df`).

## Environment / context

- Prod config: `C:\Users\camer\git\Personal Projects\ops\servers\steamboat\stacks\bootbob\bootbob.toml`
  (`http`/`tftp`/`nbd` on `10.255.0.5`, `interface = "eno1"`, `public_url`, one store
  `uberfall` at `10.255.0.3`). Compose uses `network_mode: host`, `cap_add: NET_RAW`.
- steamboat's netplan isn't in the ops repo grep for `eno1`/`vlan`; the VLAN subinterface
  will be a new ops change (staged only, per-change yes).
- Single-interface assumptions in the code today:
  - `Config.interface: Option<String>` (`backend/crates/bootbob/src/config.rs`): DHCP
    listener, mDNS, doctor probes.
  - `main.rs` binds one TFTP socket, one NBD listener, one HTTP listener, one listener task.
  - `State.addresses: HashMap<Ipv4Addr, (Mac, Millis)>` and `State::mac_for`
    (`state.rs:660`) fall back to `/proc/net/arp` without looking at the device column.
  - `ListenerStatus` is a single struct.
  - HTTP boot endpoints (`/boot/ipxe`, `/boot/disk/…`, `/boot/rollout`) take the
    machine's identity from a `mac=` query parameter, unchecked.
  - `{bootbob}`/`{server}` fill from `public_url` or the TFTP local address
    (`tftp.rs:188-197`); HTTP's `base_url` from `public_url` or `Host`.
  - The time zone (DHCP option 101) is learnt from any DHCP reply the listener sees.
  - mDNS advertises only on `interface`.

## Proposed design (awaiting Cameron)

### 1. Config

Keep today's file working unchanged: no `[[interfaces]]` means one implicit interface
built from `interface`, `http`, `tftp`, `nbd`, `public_url`, trusted, serving everything,
exactly as now.

New form:

```toml
[[interfaces]]
name = "eno1"                 # the kernel device; address discovered (or `address = …`)
label = "TSL LAN"             # optional; VLAN id is discovered
trust = "full"                # API + web UI here; any machine seen here may boot
                              # (today's behaviour)

[[interfaces]]
name = "eno1.5"
label = "Hardware CI"
trust = "boot"                # no API/UI/mDNS; only machines admitted to this interface
serve = ["tftp", "nbd", "http-boot"]   # default: everything trust allows
images = ["ci-pi"]            # optional allowlist of images it may serve
url = "http://10.5.0.5:8067"  # optional per-interface public_url
```

Ports stay global (`http`, `tftp`, `nbd` keep their meaning; with `[[interfaces]]` only
their port is used, and an IP other than unspecified is an error so it's never ambiguous).

### 2. Binding (weak-host model)

One socket per interface per service, each bound to the interface's address **and**
`SO_BINDTODEVICE` to the device. A packet for eno1.5's address that arrives on eno1 then
finds no socket, and vice versa, so the ingress interface is enforced by the kernel, not
by an address check in our code. TFTP per-transfer sockets get the same binding. NET_RAW
(already granted) covers `SO_BINDTODEVICE` on older kernels.

Each listener tags requests with its interface, so every resolver (TFTP, NBD, HTTP)
knows where a request came in. HTTP runs one axum server per interface: the full router
on `trust = "full"`, the boot-only router (`/boot/*`, `/files/ipxe|tools|cache`) on
`trust = "boot"`.

### 3. Identity per interface

- One DHCP listener per interface with listening on, each tagging what it sees.
- `addresses` keyed by `(interface, ip)`; neighbour fallback filtered on the
  `/proc/net/arp` device column. A request on eno1.5 is only ever matched against
  eno1.5's table.
- On a `boot` interface, HTTP boot requests take the MAC from the neighbour table, not
  from `mac=` (or require `mac=` to match it).
- Learning the network time zone only from `full` interfaces, so a forged DHCP reply on
  the CI VLAN can't move the rollout window.

### 4. Admission (which machines an interface serves)

Each device records where it was seen: `seen_on = { interface, vlan, at }`.
- `trust = "full"`: any machine (today's behaviour). Machines seen here are recorded.
- `trust = "boot"`: only machines admitted to that interface (`device.interfaces`
  contains it). A new MAC there shows in the UI as "seen on Hardware CI (VLAN 5), not
  admitted" and gets nothing until someone admits it from a `full` interface's UI.
  A MAC admitted only to eno1 (desky) that shows up on eno1.5 is refused and logged.
- Optional `images` allowlist per interface: resolution never hands out anything else
  there (falls through to "nothing to boot").

Honest limit: MAC and IP are spoofable *within* one L2 segment. Scoping contains that to
the segment: the CI Pi can only ever be the CI Pi.

### 5. VLAN discovery and UI

- Read `/proc/net/vlan/config` (`eno1.5 | 5 | eno1`), refreshed periodically; interfaces'
  IPv4 address/prefix via `getifaddrs`. Not netlink for now (no new dependency; can move
  later). VLANs on a VLAN-filtering bridge don't show there: shown as "no VLAN".
- `/api/status` lists interfaces: name, label, VLAN, parent, address, trust, services,
  listener counters.
- Devices page: group by interface/VLAN ("VLAN 5: Hardware CI", "Untagged: TSL LAN"), with
  each device's VLAN shown on its row.

### 6. Out of scope / follow-ups

- Image stores on a VLAN: a CI Pi using an `uberfall:` disk would connect to uberfall's
  NBD directly, which isn't on VLAN 5, and the main server can't identify it there. For
  now CI disks live on steamboat. (Question 3 below.)
- Read-only exports (ISSUES.md, `bc912f6`): separate, but the CI Pi wants it.
- Authentication on the API: still none; `full` interfaces are the trusted LAN.

## Decisions already made (don't re-ask)

- BootBob never configures host networking (Cameron's brief).
- The single-interface config keeps working (brief).
- Ops changes (`bootbob.toml`, netplan VLAN, UniFi) are staged in the ops repo for a
  per-change yes, never applied by this thread.

## Plan / steps

1. **← current** Agree the design with Cameron (questions below).
2. Config: `[[interfaces]]`, legacy synthesis, validation, tests.
3. Interface discovery: `getifaddrs` + `/proc/net/vlan/config` parser, tests.
4. Per-interface identity: tagged listener, `(iface, ip)` addresses, neighbour filter.
5. Binding: `SO_BINDTODEVICE` sockets for TFTP (incl. transfers), NBD, HTTP; interface
   context through resolvers.
6. Admission + image allowlist in TFTP, NBD, HTTP boot paths; per-interface URL filling.
7. API: interfaces in status; device `seen_on`; admit endpoint.
8. Web UI: VLAN grouping, admit button.
9. README, ISSUES.md updates.
10. Stage ops changes (bootbob.toml, netplan VLAN) for Cameron's yes.

## Findings / gotchas

- No `plans/` directory existed in this repo before this one; BootBob's design docs live
  in DeskyPi's `plans/pxe-netboot.md`.
- HTTP boot identity is the `mac=` query parameter, unverified: scoping TFTP and NBD alone
  wouldn't stop a CI job fetching `/boot/ipxe?mac=<desky>` (which also records a boot
  against desky and can use up its one-shot assignment).

## Progress log

- [x] Read README, ISSUES, config/state/tftp/nbd/http, dhcp crate, prod config
- [ ] Design agreed
- [ ] Implementation steps 2–9
- [ ] Ops changes staged

## Open questions for Cameron

1. Admission on the CI VLAN: explicit admit per machine (recommended), or any machine
   seen there?
2. API/UI only on `trust = "full"` interfaces, and MAC from the neighbour table on `boot`
   interfaces (recommended)?
3. Stores: OK that VLAN-served disks must live on steamboat for now?
4. Config shape: `trust` presets plus optional `serve` override, as above?

## Things not to do

- Don't apply anything to steamboat, UniFi or the ops checkout without a per-change yes.
- Don't rely on destination address to infer ingress interface (weak-host model).
