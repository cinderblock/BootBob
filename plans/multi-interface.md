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

## Proposed design (revised 2026-11-10 after Cameron's answers)

### 1. Config: boot interfaces and the control plane are separate

Two unrelated sets of settings (Cameron: "keep interfaces for boot separate from
interfaces for UI control. they are entirely different sets of configs"):

```toml
# Control plane: the web UI and management API. Boots nothing.
[control]
http = ["10.255.0.5:8067"]    # where the UI/API listen
mdns = ["eno1"]               # where BootBob advertises itself

# Boot interfaces: where machines netboot. No UI, no management API.
[[boot]]
interface = "eno1"            # kernel device; address and VLAN discovered
label = "TSL LAN"
services = ["dhcp-listen", "tftp", "http", "nbd"]

[[boot]]
interface = "eno1.5"
label = "Hardware CI"
services = ["tftp", "http", "nbd"]
url = "http://10.5.0.5:8067"  # optional: what machines here are told to fetch from
```

The old single-interface keys are refused in server mode (breaking change, approved).

Open (question 5): a boot interface and the control listener on the same address and
port. Proposed: allowed, served by one socket with both route sets, so every existing
machine, USB stick and `bootbob.url=` keeps working. A boot interface with no control
listener on it gets only the boot routes (`/boot/*`, `/files/*`).

### 2. Binding (weak-host model)

One socket per boot interface per service, bound to the interface's address **and**
`SO_BINDTODEVICE`, so the kernel drops anything that arrives on the wrong interface. TFTP
per-transfer sockets get the same binding. Each request carries its boot interface into
the resolvers.

### 3. Identity per interface

- One DHCP listener per boot interface with `dhcp-listen`, tagging what it sees.
- `addresses` keyed by `(interface, ip)`; neighbour fallback filtered on the device.
- HTTP boot requests take the MAC from the neighbour table on that interface; a `mac=`
  that disagrees is refused and logged.
- The network time zone is learnt only from the listener on the control interface (or a
  configured one), never a CI VLAN.

### 4. Admission: unknown machines are held at the menu

Cameron: an unadmitted machine "should be held at our boot menu until a local user or
remote admin tells it what to do", and "unauthorized hosts only get access to public
images".

- Images get a `public` flag. The built-in menu and local-disk boot are public.
- An **unadmitted** machine gets the fallback menu, listing only public images. Someone
  at its keyboard can pick one (for that boot). An admin can admit it and/or assign it
  something from the UI.
- An **admitted** machine is admitted to particular boot interfaces. A machine admitted
  only to eno1 (desky) that turns up on eno1.5 is treated as unadmitted there: menu,
  public images only, logged.
- NBD and Pi TFTP serve an unadmitted machine only the disks of public images.
- Open: Pis have no menu (question 6), whether admission applies on eno1 too
  (question 7).

Honest limit: MAC and IP are spoofable within one L2 segment. Scoping keeps that inside
the segment.

### 5. VLAN discovery and UI

- `/proc/net/vlan/config` for VLAN id and parent; `getifaddrs` for addresses. VLANs on a
  VLAN-filtering bridge don't show there: shown as "no VLAN".
- `/api/status` lists boot interfaces (name, label, VLAN, parent, address, services,
  listener counters) and the control listeners.
- Devices page grouped by VLAN ("VLAN 5: Hardware CI"), with admit/unadmit per interface.

### 6. Out of scope / follow-ups

- Image stores on a VLAN: a CI Pi using an `uberfall:` disk would connect to uberfall's
  NBD directly, which isn't on VLAN 5. For now VLAN-served disks live on steamboat.
- Read-only exports (ISSUES.md, `bc912f6`).
- Authentication on the control API: still none; it's reachable only where `[control]`
  listens.

## Decisions already made (don't re-ask)

- BootBob never configures host networking (Cameron's brief).
- The single-interface config keeps working (brief).
- Ops changes (`bootbob.toml`, netplan VLAN, UniFi) are staged in the ops repo for a
  per-change yes, never applied by this thread.
- Boot interfaces and the control plane (UI/API) are entirely separate config sets
  (Cameron, 2026-11-10).
- Unadmitted machines are held at BootBob's boot menu until a local user or remote admin
  says what to do; they get only public images (Cameron, 2026-11-10).
- An unadmitted Pi gets nothing and shows as waiting until an admin acts (Cameron).
- Unadmitted-is-held is the default on **every** boot interface, eno1 included (Cameron).
  Machines already in the database are admitted to the first boot interface on upgrade, so
  desky and friends keep booting, but aren't admitted to new VLANs.
- Control and a boot interface may share one address:port; one socket serves both route
  sets (Cameron: "sounds fine").
- Breaking config changes are fine: "no existing machines" depend on the old shape
  (Cameron). Server mode moves to `[control]` + `[[boot]]`; the old `interface`/`http`/
  `tftp`/`nbd`/`public_url`/`mdns` keys are refused in server mode with a message saying
  what replaces them. Store mode keeps `http`/`nbd`.
- Assigning an image to an unadmitted machine also admits it, to the interface it was last
  seen on (Cameron).

## Plan / steps

1. Agree the design with Cameron. Done.
2. **← current** Config: `[[interfaces]]`, legacy synthesis, validation, tests.
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
- [x] Design agreed (2026-11-10), except question 3 (stores), which only affects the ops step
- [ ] Implementation steps 2–9
- [ ] Ops changes staged

## Open questions for Cameron

1. ~~Admission policy~~: answered, held at the menu, public images only.
2. ~~API/UI on restricted interfaces~~: answered, separate config sets.
3. Stores: OK that VLAN-served disks must live on steamboat for now? (Recommend yes.)
4. ~~Config shape~~: superseded by `[control]` + `[[boot]]`.
5. ~~Shared address:port~~: yes.
6. ~~Unadmitted Pi~~: nothing, waiting.
7. ~~eno1 too~~: yes, unadmitted-is-held everywhere by default.
8. ~~Assign admits~~: yes.

## Things not to do

- Don't apply anything to steamboat, UniFi or the ops checkout without a per-change yes.
- Don't rely on destination address to infer ingress interface (weak-host model).
