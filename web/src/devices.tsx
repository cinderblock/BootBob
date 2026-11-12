import { useEffect, useState } from "react";
import { api, type BootEvent, type BootInterface, type Device, type Overlay } from "./api";
import type { Live } from "./live";
import { ago, fullTime } from "./time";

const stateLabel: Record<Device["state"], string> = {
  fetching: "waiting for a download",
  at_menu: "waiting at menu",
  menu_idle: "at menu, idle",
  pi_waiting: "Pi, nothing to boot",
  stuck: "didn't reach us",
  booted: "booted",
  seen: "seen",
};

function Ago({ ms, now }: { ms: number | null | undefined; now: number }) {
  if (!ms) return <span className="muted">never</span>;
  return <time title={fullTime(ms)}>{ago(ms, now)}</time>;
}

/** The boot interface a machine belongs under: where it was seen last, else where it's admitted. */
export function homeOf(d: Device): string | null {
  const seen = Object.entries(d.seen ?? {}).sort((a, b) => b[1].at - a[1].at);
  return seen[0]?.[0] ?? d.admitted?.[0] ?? null;
}

/** "VLAN 5 · Hardware CI", or the label alone for an untagged interface. */
export function ifaceName(i: BootInterface | undefined, name: string): string {
  if (!i) return name;
  return i.vlan !== null ? `VLAN ${i.vlan} · ${i.label}` : i.label;
}

export function Devices({ live, now }: { live: Live; now: number }) {
  const [open, setOpen] = useState<string | null>(null);
  const ifaces = live.status?.interfaces ?? [];
  if (live.devices.length === 0) {
    return (
      <section className="empty">
        <h2>No machines yet</h2>
        <p>
          Machines appear here the first time they netboot from BootBob, or when the DHCP listener sees them try. Point a PC's firmware
          at the network boot, or see the README for the network's boot settings.
        </p>
      </section>
    );
  }
  // One group per boot interface (so per VLAN), in the config's order, then anything else.
  const groups: { key: string; title: string; detail: string | null; devices: Device[] }[] = ifaces.map((i) => ({
    key: i.interface,
    title: ifaceName(i, i.interface),
    detail: [i.interface, i.address].filter(Boolean).join(", "),
    devices: [],
  }));
  for (const d of live.devices) {
    const home = homeOf(d);
    let g = groups.find((g) => g.key === (home ?? ""));
    if (!g) {
      g = { key: home ?? "", title: home ?? "Not seen on a boot interface yet", detail: null, devices: [] };
      groups.push(g);
    }
    g.devices.push(d);
  }
  return (
    <section>
      {groups
        .filter((g) => g.devices.length > 0)
        .map((g) => (
          <div key={g.key} className="device-group">
            <h2 className="group-title">
              {g.title} <span className="muted small">{g.detail && `${g.detail} · `}{g.devices.length} machines</span>
            </h2>
            <table className="devices">
              <thead>
                <tr>
                  <th>Machine</th>
                  <th>State</th>
                  <th>Address</th>
                  <th>Firmware</th>
                  <th>Next boot</th>
                  <th>Last seen</th>
                </tr>
              </thead>
              <tbody>
                {g.devices.map((d) => (
                  <DeviceRow
                    key={d.mac}
                    d={d}
                    home={g.key}
                    live={live}
                    now={now}
                    open={open === d.mac}
                    toggle={() => setOpen(open === d.mac ? null : d.mac)}
                  />
                ))}
              </tbody>
            </table>
          </div>
        ))}
    </section>
  );
}

function DeviceRow({ d, home, live, now, open, toggle }: { d: Device; home: string; live: Live; now: number; open: boolean; toggle: () => void }) {
  const model = [d.manufacturer, d.product].filter(Boolean).join(" ");
  const admitted = (d.admitted ?? []).includes(home);
  const ip = d.seen?.[home]?.ip ?? d.ip;
  return (
    <>
      <tr className={open ? "row open" : "row"} onClick={toggle}>
        <td>
          <strong>{d.name ?? d.code}</strong>
          {d.name && <span className="muted"> {d.code}</span>}
          <div className="muted small">{model || d.mac}</div>
        </td>
        <td>
          <span className={`state ${d.state}`}>{stateLabel[d.state]}</span>
          {home && !admitted && (
            <span className="tag warn" title="Held at the menu with public images only, until someone admits it">
              not admitted
            </span>
          )}
          {d.state === "at_menu" && d.menu && (
            <div className="muted small">
              checked in <Ago ms={d.menu.last_poll} now={now} />
            </div>
          )}
        </td>
        <td>{ip ?? <span className="muted">unknown</span>}</td>
        <td>{d.firmware ?? <span className="muted">unknown</span>}</td>
        <td>
          {d.next.title}
          {d.assignment?.once && <span className="tag">once</span>}
          {d.assignment?.track && <span className="tag">follows {d.assignment.track}</span>}
        </td>
        <td>
          <Ago ms={d.last_seen} now={now} />
        </td>
      </tr>
      {open && (
        <tr className="detail">
          <td colSpan={6}>
            <DeviceDetail d={d} live={live} now={now} />
          </td>
        </tr>
      )}
    </>
  );
}

function fmtBytes(n: number): string {
  if (n < 1 << 20) return `${(n / 1024).toFixed(0)} KB`;
  if (n < 1 << 30) return `${(n / (1 << 20)).toFixed(0)} MB`;
  return `${(n / (1 << 30)).toFixed(1)} GB`;
}

function DeviceDetail({ d, live, now }: { d: Device; live: Live; now: number }) {
  const [image, setImage] = useState(d.assignment?.image ?? "");
  const [once, setOnce] = useState(d.assignment?.once ?? d.state === "at_menu");
  const [name, setName] = useState(d.name ?? "");
  const [notes, setNotes] = useState(d.notes ?? "");
  const [error, setError] = useState<string | null>(null);
  const [events, setEvents] = useState<BootEvent[]>([]);
  const [confirmForget, setConfirmForget] = useState(false);
  const [overlays, setOverlays] = useState<Overlay[]>([]);
  const [track, setTrack] = useState(d.assignment?.track ?? "");

  // This machine's log, refreshed whenever the global log moves.
  useEffect(() => {
    api.events(d.mac).then(setEvents, () => {});
  }, [d.mac, live.events.length, live.events[live.events.length - 1]?.at]);

  // Its copy-on-write overlays, refreshed with the log (connects and resets land there).
  const isPi = d.firmware === "rpi";
  useEffect(() => {
    if (isPi) api.overlays(d.mac).then(setOverlays, () => setOverlays([]));
  }, [d.mac, isPi, live.events.length, live.events[live.events.length - 1]?.at]);

  const run = (p: Promise<unknown>) => p.then(() => setError(null), (e: Error) => setError(e.message));
  // A Pi boots only images with Pi boot files; a PC only those with an iPXE action.
  const bootable = live.images.filter((i) => (isPi ? i.pi !== null : i.action !== null || i.disk !== null || i.id === "menu"));
  // Tracks of its kind: a version boots either Pis or PCs.
  const followable = live.tracks.filter((t) => {
    const current = live.images.find((i) => i.id === t.current);
    return current && (isPi ? current.pi !== null : current.disk !== null);
  });

  return (
    <div className="device-detail">
      <div className="col">
        <h3>What it boots</h3>
        <p>
          {d.summary}.{" "}
          <span className="muted">
            Next netboot: <strong>{d.next.title}</strong> ({d.next.reason}).
          </span>
        </p>
        <form
          className="assign"
          onSubmit={(e) => {
            e.preventDefault();
            if (image) run(api.assign(d.mac, { image, once }));
          }}
        >
          <select value={image} onChange={(e) => setImage(e.currentTarget.value)}>
            <option value="">Choose an image…</option>
            {bootable.map((i) => (
              <option key={i.id} value={i.id}>
                {i.title}
              </option>
            ))}
          </select>
          <label title="Boot it once, then go back to the usual (assignment, or the default)">
            <input type="checkbox" checked={once} onChange={(e) => setOnce(e.currentTarget.checked)} /> next boot only
          </label>
          <button type="submit" disabled={!image}>
            {d.state === "at_menu" ? "Boot it now" : "Assign"}
          </button>
          {d.assignment && (
            <button type="button" className="secondary" onClick={() => run(api.unassign(d.mac))}>
              Clear assignment
            </button>
          )}
        </form>
        {d.state === "at_menu" && <p className="muted small">It's waiting at the menu, so it picks this up at its next check-in.</p>}
        {followable.length > 0 && (
          <form
            className="assign"
            onSubmit={(e) => {
              e.preventDefault();
              if (track) run(api.assign(d.mac, { image: "", once: false, track }));
            }}
          >
            <select value={track} onChange={(e) => setTrack(e.currentTarget.value)}>
              <option value="">Follow a track…</option>
              {followable.map((t) => (
                <option key={t.id} value={t.id}>
                  {t.title}
                </option>
              ))}
            </select>
            <button type="submit" disabled={!track || d.assignment?.track === track} title="Its root is replaced by each new version, and it reboots itself in the window. Only for machines that keep their state elsewhere.">
              Follow
            </button>
          </form>
        )}
        {d.check && (
          <p className="muted small">
            Checked in <Ago ms={d.check.at} now={now} />: {d.check.health}
            {d.check.busy && `, busy (${d.check.busy})`}.
          </p>
        )}
        {error && <p className="error">{error}</p>}

        <h3>Admitted on</h3>
        <p className="muted small">
          Where it may boot what it's assigned. Anywhere else it's held at the menu with public images only, and a Pi gets nothing.
          Assigning it an image admits it where it was seen last.
        </p>
        <ul className="admission">
          {(live.status?.interfaces ?? []).map((i) => {
            const on = (d.admitted ?? []).includes(i.interface);
            const seen = d.seen?.[i.interface];
            return (
              <li key={i.interface}>
                <label>
                  <input type="checkbox" checked={on} onChange={(e) => run(api.admit(d.mac, i.interface, e.currentTarget.checked))} />{" "}
                  {ifaceName(i, i.interface)} <span className="muted small">({i.interface})</span>
                </label>
                {seen && (
                  <span className="muted small">
                    {" "}
                    seen <Ago ms={seen.at} now={now} />
                    {seen.ip && ` at ${seen.ip}`}
                  </span>
                )}
              </li>
            );
          })}
        </ul>

        {overlays.length > 0 && (
          <>
            <h3>Its own changes</h3>
            <p className="muted small">
              Copy-on-write overlays: what this machine has written over a shared image. Resetting one puts it back on the
              pristine image at its next boot.
            </p>
            <ul className="overlays">
              {overlays.map((o) => (
                <li key={o.source}>
                  <strong>{o.source}</strong> <span className="muted">{fmtBytes(o.allocated)} written</span>{" "}
                  {o.in_use ? (
                    <span className="tag">in use</span>
                  ) : (
                    <button className="secondary" onClick={() => run(api.resetOverlay(d.mac, o.source))}>
                      Reset to image
                    </button>
                  )}
                </li>
              ))}
            </ul>
          </>
        )}

        <h3>Name and notes</h3>
        <form
          className="naming"
          onSubmit={(e) => {
            e.preventDefault();
            run(api.patchDevice(d.mac, { name: name || null, notes: notes || null }));
          }}
        >
          <input placeholder={d.code} value={name} onInput={(e) => setName(e.currentTarget.value)} />
          <input placeholder="Notes" value={notes} onInput={(e) => setNotes(e.currentTarget.value)} />
          <button type="submit">Save</button>
        </form>
      </div>

      <div className="col">
        <h3>Details</h3>
        <dl>
          <dt>MAC</dt>
          <dd>{d.mac}</dd>
          {d.uuid && (
            <>
              <dt>UUID</dt>
              <dd>{d.uuid}</dd>
            </>
          )}
          {d.serial && (
            <>
              <dt>{isPi ? "Pi serial" : "Serial"}</dt>
              <dd>{d.serial}</dd>
            </>
          )}
          <dt>First seen</dt>
          <dd>{fullTime(d.first_seen)}</dd>
          {d.dhcp && (
            <>
              <dt>Last netboot try</dt>
              <dd>
                <Ago ms={d.dhcp.last_discover} now={now} /> ({d.dhcp.vendor_class ?? "no vendor class"})
              </dd>
              {d.dhcp.offer && (
                <>
                  <dt>DHCP told it</dt>
                  <dd>
                    boot server {d.dhcp.offer.boot_server ?? "(none)"}, file {d.dhcp.offer.boot_file ?? "(none)"}
                    {d.dhcp.offer.server && <span className="muted"> (from {d.dhcp.offer.server})</span>}
                  </dd>
                </>
              )}
            </>
          )}
          {d.tftp && (
            <>
              <dt>Last TFTP</dt>
              <dd>
                {d.tftp.path}
                {d.tftp.served && d.tftp.served !== d.tftp.path && <> → {d.tftp.served}</>}
                {d.tftp.ok === false && <span className="error"> (failed)</span>} <Ago ms={d.tftp.at} now={now} />
              </dd>
            </>
          )}
        </dl>
        <h3>Log</h3>
        <ul className="events compact">
          {events
            .slice(-12)
            .reverse()
            .map((e, i) => (
              <li key={i}>
                <Ago ms={e.at} now={now} /> {e.message}
              </li>
            ))}
        </ul>
        <button
          className={confirmForget ? "danger" : "secondary"}
          onClick={() => (confirmForget ? run(api.forgetDevice(d.mac)) : setConfirmForget(true))}
          onBlur={() => setConfirmForget(false)}
        >
          {confirmForget ? "Click again to forget it" : "Forget this machine"}
        </button>
      </div>
    </div>
  );
}
