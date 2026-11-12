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
  NBD directly, which isn't on the VLAN. Cameron (2026-11-11): "Steamboat should serve the
  boot images as needed." Proposed: steamboat relays NBD from the store for a boot
  interface that can't reach it (question 3).
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
2. Config: `[control]` + `[[boot]]`, old keys refused with what replaces them. Written.
3. Interface discovery: `netif.rs` (`getifaddrs`, `/proc/net/vlan/config`). Written.
4. Per-interface identity: listener per interface, `(iface, ip)` addresses,
   `neigh::lookup_on`. Written.
5. Binding: `SO_BINDTODEVICE` on TFTP (and each transfer socket), NBD, boot HTTP and
   control HTTP (bound to the interface holding its address). Written.
6. Admission and public images in TFTP, NBD, HTTP boot paths; per-interface URLs. Written.
7. API: interfaces in `/api/status`; device `admitted`/`seen`; admit endpoints. Written.
8. Web UI: machines grouped by interface/VLAN, "not admitted" tag, admit checkboxes, image
   `public` flag, a status chip per interface. Written.
9. Build and test on Windows and Linux (WSL), web typecheck and tests, README, ISSUES.md,
   commit. Done.
10. Ops, each change only with Cameron's yes:
    a. **Done 2026-11-11.** `bootbob.toml` in the new shape (draft: `plans/ops-draft-steamboat-bootbob.toml`),
       landing in the same ops commit as the `pin.json` bump to a build of this branch:
       the new BootBob refuses the old file and the old one refuses the new.
    b. **← current, waiting on Cameron.** The CI VLAN: a UniFi network (VLAN id and subnet: the CI thread's or Cameron's
       call), a tagged subinterface on steamboat (netplan isn't in ops today, only a
       networkd drop-in), and a second `[[boot]]` section. The ops UniFi tooling sets
       DHCP/netboot/hosts/policies but doesn't appear to create networks.
    c. Caddy: no change. It runs with host networking, so its connection to
       10.255.0.5:8067 is local, which a socket bound to eno1 still accepts (tested).

## Findings / gotchas

- No `plans/` directory existed in this repo before this one; BootBob's design docs live
  in DeskyPi's `plans/pxe-netboot.md`.
- HTTP boot identity was the `mac=` query parameter, unverified: scoping TFTP and NBD alone
  wouldn't stop a CI job fetching `/boot/ipxe?mac=<desky>`. Now the MAC comes from the
  interface's neighbour table and `mac=` must agree.
- A Pi's MAC path prefix (`dc-a6-32-…/start4.elf`) used to *override* identity, so anything
  could fetch another Pi's boot files by naming it. Now it must agree with the neighbour
  table. Consequence: routed Pis (TFTP from another subnet, `TFTP_PREFIX=2`) can't be
  identified any more. They should be on a boot interface of their own.
- `/boot/disk/{image}/…` (PC kernels/initrds) had no access check at all. Now: admitted
  here, or a public image.
- **Weak-host model, tested on a real kernel (WSL2 6.18, 2026-11-10):** a socket bound only
  to `10.9.2.1` (on `dum0`) accepted a TCP connection and a UDP packet that arrived on
  `va1`. With `SO_BINDTODEVICE dum0`, both were refused/dropped. Script:
  `tools/test-weak-host.sh` (throwaway netns, cleaned up).
- **Local connections still work with device binding:** a socket bound to `eth0` accepts
  the host's own connection to `eth0`'s address (it arrives on `lo`, but the kernel matches
  it to `eth0`). So Caddy on steamboat proxying to `10.255.0.5:8067` keeps working; a socket
  bound to `lo` does *not* accept connections to `eth0`'s address.
- **Renovate deploys BootBob by itself.** ops' Renovate (daily at 4:00 AM PST / 12:00Z,
  `renovate.json` "App deploys") follows `ghcr.io/cinderblock/bootbob:latest` and
  fast-forwards ops master with the new pin, which deploys. So every BootBob master push
  publishes and then deploys within a day, and a change that breaks the prod config must
  get its ops commit in before the next Renovate run. It also means a plan-only push to
  master ships a new image; plan updates stay on this branch until there's code to merge.
- `sentinel` can't build BootBob (not on its allowed list); local builds queue behind
  other threads' jobs in the 4-slot budget, often for 15+ minutes.

## Progress log

- [x] Read README, ISSUES, config/state/tftp/nbd/http, dhcp crate, prod config
- [x] Design agreed (2026-11-10), except question 3 (stores), which only affects the ops step
- [x] Implementation steps 2–8
- [x] Windows `cargo test --workspace`: all pass (111 in bootbob)
- [x] Linux (WSL2) `cargo fmt --check`, `cargo clippy --all-targets -D warnings`,
      `cargo test --workspace`: all pass
- [x] Web `bun run typecheck` and `bun run test`: pass
- [x] New tests (`admission_tests.rs`): spoofed `mac=`, spoofed Pi prefix, unadmitted menu
      shows public only, unadmitted Pi gets nothing, admitted-elsewhere machine refused NBD
      and TFTP, assign admits, upgrade migration, status lists interfaces, control and boot
      routes apart, forged DHCP time zone ignored off the control network
- [x] ISSUES.md: "Anything on the LAN…" narrowed to "The control API has no authentication"
- [x] Commit
- [x] Merged PR #1 (`3a54e5d`, 2026-11-11 4:33 PM PST); master CI passed and published
      `sha256:37a7e855…`
- [x] Ops: `bootbob.toml` + pin in ops `34a3e82b`, deployed by
      https://github.com/cinderblock/ops/actions/runs/43131104591. Checked after: steamboat
      reports eno1 "TSL LAN", listener running, all 3 known machines admitted on eno1, and
      http://pxe.tsl answers 200 through Caddy. The new build refuses an old-style config
      with the intended message (checked locally).
- [ ] Ops: CI VLAN (UniFi, netplan, second `[[boot]]`), not started: needs VLAN id/subnet
- [x] e2e (PR CI) and rollout (pc-golden, dispatched on the branch) pass
- [ ] Web UI looked at in a browser (only typechecked so far)

## Open questions for Cameron

1. ~~Admission policy~~: answered, held at the menu, public images only.
2. ~~API/UI on restricted interfaces~~: answered, separate config sets.
3. Stores: Cameron: "Steamboat should serve the boot images as needed." Proposed: for a
   boot interface that can't reach a store, steamboat relays NBD to the store (one copy,
   overlays stay on the store), rather than copying the disk. Awaiting his yes.
4. ~~Config shape~~: superseded by `[control]` + `[[boot]]`.
5. ~~Shared address:port~~: yes.
6. ~~Unadmitted Pi~~: nothing, waiting.
7. ~~eno1 too~~: yes, unadmitted-is-held everywhere by default.
8. ~~Assign admits~~: yes.
9. ~~Push for CI~~: yes (2026-11-11). Rebased onto origin/master, pushed, and opened draft
   PR https://github.com/cinderblock/BootBob/pull/1, since CI runs on pull requests, not
   branch pushes. The repo is public, so CI is free. All 6 PR checks passed on c8e45c8
   (backend, web, ipxe, docker, e2e; publish skipped, it runs only on master). The rollout
   test (pc-golden.yml) runs only on master or by hand, so it was dispatched on the branch:
   https://github.com/cinderblock/BootBob/actions/runs/43124221939 passed, 2026-11-11
   4:20 PM PST (00:20Z). Next: Cameron's yes to merge (master publishes the image), then the
   ops change with its own yes. Both given and done.
10. CI VLAN: Cameron (2026-11-11): id and subnet undecided; work with the ops repo to pick
    and set up an isolated VLAN, maybe in a new thread. It must hold several hosts isolated
    from each other (the new M1 Mac goes on it, and the Test Pi), but Cameron needs some
    way to send his own LAN jobs to the M1 for builds (less so the Test Pi). Proposed: a
    new thread for the network design in ops; BootBob's part is a second `[[boot]]`.
    Cameron said ok, and added (2026-11-11): "Uberfall should not be reachable from VLAN",
    and isolated VLANs must count as internet in steamboat's configs (notably Caddy),
    because LAN privilege is decided by local source IPs. M1 access from his LAN: an extra
    vNIC on his PC, or routing to him only. Thread started:
    t3-thread://v2/ab3781d7-a1dd-4bbb-b3e9-6331e19b7569/mcp:1347b6d1-a8b0-4ab2-a521-ba2a282edebc
    (ops, branch `ci-vlan`).

## Things not to do

- Don't apply anything to steamboat, UniFi or the ops checkout without a per-change yes.
- Don't rely on destination address to infer ingress interface (weak-host model).
- Don't bind the control listener to `lo` expecting it to serve the LAN address: bind it to
  the LAN address (device-bound to `eno1`), plus `127.0.0.1` if a local proxy uses that.
