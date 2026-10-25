import { useEffect, useState } from "react";
import { api, type BootEvent, type Device, type Overlay } from "./api";
import type { Live } from "./live";
import { ago, fullTime } from "./time";

const stateLabel: Record<Device["state"], string> = {
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

export function Devices({ live, now }: { live: Live; now: number }) {
  const [open, setOpen] = useState<string | null>(null);
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
  return (
    <section>
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
          {live.devices.map((d) => (
            <DeviceRow key={d.mac} d={d} live={live} now={now} open={open === d.mac} toggle={() => setOpen(open === d.mac ? null : d.mac)} />
          ))}
        </tbody>
      </table>
    </section>
  );
}

function DeviceRow({ d, live, now, open, toggle }: { d: Device; live: Live; now: number; open: boolean; toggle: () => void }) {
  const model = [d.manufacturer, d.product].filter(Boolean).join(" ");
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
          {d.state === "at_menu" && d.menu && (
            <div className="muted small">
              checked in <Ago ms={d.menu.last_poll} now={now} />
            </div>
          )}
        </td>
        <td>{d.ip ?? <span className="muted">unknown</span>}</td>
        <td>{d.firmware ?? <span className="muted">unknown</span>}</td>
        <td>
          {d.next.title}
          {d.assignment?.once && <span className="tag">once</span>}
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
  const bootable = live.images.filter((i) => (isPi ? i.pi !== null : i.action !== null || i.id === "menu"));

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
        {error && <p className="error">{error}</p>}

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
