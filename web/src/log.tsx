import type { BootEvent, Device } from "./api";
import { ago, fullTime } from "./time";

export function Log({ events, devices, now }: { events: BootEvent[]; devices: Device[]; now: number }) {
  const names = new Map(devices.map((d) => [d.mac, d.name ?? d.code]));
  return (
    <section>
      <h2>Log</h2>
      <ul className="events">
        {[...events].reverse().map((e, i) => (
          <li key={i} className={`kind-${e.kind}`}>
            <time title={fullTime(e.at)}>{ago(e.at, now)}</time>
            <span className="who">{e.mac ? (names.get(e.mac) ?? e.mac) : "server"}</span>
            <span>{e.message}</span>
          </li>
        ))}
      </ul>
    </section>
  );
}
