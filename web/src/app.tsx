import { useState } from "react";
import type { BootInterface } from "./api";
import { Catalogue } from "./catalogue";
import { Devices } from "./devices";
import { Doctor } from "./doctor";
import { Images } from "./images";
import { useLive, useNow } from "./live";
import { Log } from "./log";
import { Rollouts } from "./rollouts";
import { SettingsView } from "./settings";

const tabs = ["Machines", "Images", "Rollouts", "Catalogue", "Network check", "Settings", "Log"] as const;
type Tab = (typeof tabs)[number];

function ifaceChip(i: BootInterface): string {
  if (!i.address) return "chip bad";
  if (i.listener && !i.listener.running) return "chip warn";
  return "chip ok";
}

function ifaceTitle(i: BootInterface, bootFile: string): string {
  const lines = [
    `${i.interface}${i.parent ? ` on ${i.parent}` : ""}${i.address ? `, ${i.address}` : ", waiting for the interface"}`,
    `Serves: ${i.services.join(", ")}`,
    `Boot file name: ${bootFile}`,
    `${i.admitted} machines admitted`,
  ];
  if (i.listener) {
    lines.push(
      i.listener.running
        ? `DHCP listener: ${i.listener.pxe_seen} PXE requests of ${i.listener.dhcp_seen} DHCP messages seen`
        : `DHCP listener stopped${i.listener.error ? `: ${i.listener.error}` : ""}`,
    );
  }
  return lines.join("\n");
}

export function App() {
  const live = useLive();
  const now = useNow();
  const [tab, setTab] = useState<Tab>("Machines");
  const s = live.status;
  const waiting = live.devices.filter((d) => d.state === "at_menu").length;

  return (
    <div className="page">
      <header>
        <h1>BootBob</h1>
        <nav>
          {tabs.map((t) => (
            <button key={t} className={t === tab ? "tab active" : "tab"} onClick={() => setTab(t)}>
              {t}
              {t === "Machines" && waiting > 0 && <span className="count">{waiting}</span>}
            </button>
          ))}
        </nav>
        <div className="chips">
          <span className={live.connected ? "chip ok" : "chip bad"} title="Live updates from the server">
            {live.connected ? "live" : "reconnecting"}
          </span>
          {s && (
            <>
              {s.interfaces.length === 0 && (
                <span className="chip bad" title="Add a [[boot]] section to the config">
                  no boot interfaces
                </span>
              )}
              {s.interfaces.map((i) => (
                <span key={i.interface} className={ifaceChip(i)} title={ifaceTitle(i, s.boot_filename)}>
                  {i.label}
                  {i.vlan !== null && ` · VLAN ${i.vlan}`}
                </span>
              ))}
              <span className={s.ipxe_builds.length ? "chip ok" : "chip bad"} title={s.ipxe_builds.join("\n") || "No iPXE builds found"}>
                iPXE {s.ipxe_builds.length ? `${s.ipxe_builds.length} builds` : "missing"}
              </span>
            </>
          )}
        </div>
      </header>
      {live.error && <div className="banner">Can't reach BootBob: {live.error}</div>}
      <main>
        {tab === "Machines" && <Devices live={live} now={now} />}
        {tab === "Images" && <Images live={live} />}
        {tab === "Rollouts" && <Rollouts live={live} now={now} />}
        {tab === "Catalogue" && <Catalogue live={live} now={now} />}
        {tab === "Network check" && <Doctor now={now} />}
        {tab === "Settings" && <SettingsView live={live} />}
        {tab === "Log" && <Log events={live.events} devices={live.devices} now={now} />}
      </main>
      <footer>{s && `BootBob ${s.version}`}</footer>
    </div>
  );
}
