import { useState } from "react";
import { Devices } from "./devices";
import { Images } from "./images";
import { useLive, useNow } from "./live";
import { Log } from "./log";
import { SettingsView } from "./settings";

const tabs = ["Machines", "Images", "Settings", "Log"] as const;
type Tab = (typeof tabs)[number];

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
              <span className={s.tftp ? "chip ok" : "chip warn"} title={`Boot file name: ${s.boot_filename}`}>
                TFTP {s.tftp ?? "off"}
              </span>
              <span
                className={s.listener.running ? "chip ok" : s.listener.interface ? "chip bad" : "chip"}
                title={s.listener.error ?? `${s.listener.pxe_seen} PXE requests of ${s.listener.dhcp_seen} DHCP messages seen`}
              >
                DHCP listener {s.listener.running ? `on ${s.listener.interface}` : s.listener.interface ? "stopped" : "off"}
              </span>
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
        {tab === "Settings" && <SettingsView live={live} />}
        {tab === "Log" && <Log events={live.events} devices={live.devices} now={now} />}
      </main>
      <footer>{s && `BootBob ${s.version}`}</footer>
    </div>
  );
}
