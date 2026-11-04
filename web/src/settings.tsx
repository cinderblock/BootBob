import { useEffect, useState } from "react";
import { api, type Settings, type Theme } from "./api";
import type { Live } from "./live";

const themeFields: { key: keyof Theme; label: string }[] = [
  { key: "background", label: "Background" },
  { key: "surface", label: "Panels" },
  { key: "text", label: "Text" },
  { key: "muted", label: "Secondary text" },
  { key: "accent", label: "Accent" },
];

const browserZone = Intl.DateTimeFormat().resolvedOptions().timeZone;

export function SettingsView({ live }: { live: Live }) {
  const [s, setS] = useState<Settings | null>(live.settings);
  const [saved, setSaved] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    if (!s && live.settings) setS(live.settings);
  }, [live.settings]);
  if (!s) return null;
  const setTheme = (k: keyof Theme, v: string) => setS({ ...s, theme: { ...s.theme, [k]: v } });

  return (
    <section>
      <form
        className="settings"
        onSubmit={(e) => {
          e.preventDefault();
          api.putSettings(s).then(
            () => {
              setError(null);
              setSaved("Saved");
              // The page's own colours come from /theme.css.
              const link = document.querySelector<HTMLLinkElement>('link[href^="/theme.css"]');
              if (link) link.href = `/theme.css?${Date.now()}`;
            },
            (err: Error) => {
              setSaved(null);
              setError(err.message);
            },
          );
        }}
      >
        <h2>Booting</h2>
        <label>
          Machines with no assignment get{" "}
          <select value={s.default_image} onChange={(e) => setS({ ...s, default_image: e.currentTarget.value })}>
            {live.images
              .filter((i) => i.action !== null || i.disk !== null || i.id === "menu")
              .map((i) => (
                <option key={i.id} value={i.id}>
                  {i.title}
                </option>
              ))}
          </select>
        </label>
        <label title="A Pi with nothing to boot keeps asking (with network-only BOOT_ORDER), and shows up in Machines.">
          Raspberry Pis with no assignment get{" "}
          <select value={s.pi_default_image} onChange={(e) => setS({ ...s, pi_default_image: e.currentTarget.value })}>
            <option value="">Nothing</option>
            {live.images
              .filter((i) => i.pi !== null)
              .map((i) => (
                <option key={i.id} value={i.id}>
                  {i.title}
                </option>
              ))}
          </select>
        </label>
        <label>
          The fallback menu checks for an assignment every{" "}
          <input
            type="number"
            min={3}
            max={300}
            value={s.poll_secs}
            onInput={(e) => setS({ ...s, poll_secs: Number(e.currentTarget.value) })}
          />{" "}
          seconds
        </label>
        <label title="So a PC that only tries the network before its disk still starts normally. A key at the menu stops the clock.">
          If nobody chooses, boot the local disk after{" "}
          <input
            type="number"
            min={0}
            value={s.menu_timeout_secs}
            onInput={(e) => setS({ ...s, menu_timeout_secs: Number(e.currentTarget.value) })}
          />{" "}
          seconds (0: wait forever)
        </label>

        <h2>Rollouts</h2>
        <label title="Machines following a track move to a new version one at a time in this window, every day.">
          Machines may move to a new version from{" "}
          <input type="time" value={s.rollout_start} onInput={(e) => setS({ ...s, rollout_start: e.currentTarget.value })} /> for{" "}
          <input
            type="number"
            min={1}
            max={1440}
            value={s.rollout_minutes}
            onInput={(e) => setS({ ...s, rollout_minutes: Number(e.currentTarget.value) })}
          />{" "}
          minutes
        </label>
        <label title="Past this, the machine goes back to the version it came from and the rollout stops. Set it longer than its slowest healthy boot.">
          A machine has{" "}
          <input
            type="number"
            min={1}
            max={240}
            value={s.rollout_health_minutes}
            onInput={(e) => setS({ ...s, rollout_health_minutes: Number(e.currentTarget.value) })}
          />{" "}
          minutes to come back healthy
        </label>
        <label>
          In the time zone{" "}
          <input
            value={s.time_zone}
            placeholder={s.network_time_zone ?? "UTC"}
            onInput={(e) => setS({ ...s, time_zone: e.currentTarget.value })}
          />{" "}
          {s.time_zone === "" ? (
            <span className="muted">{s.network_time_zone ? "from the network" : "the network gives none"}</span>
          ) : (
            s.network_time_zone && (
              <button type="button" className="secondary" onClick={() => setS({ ...s, time_zone: "" })}>
                Use the network's
              </button>
            )
          )}{" "}
          {s.time_zone !== browserZone && (
            <button type="button" className="secondary" onClick={() => setS({ ...s, time_zone: browserZone })}>
              Use {browserZone}
            </button>
          )}
        </label>

        <h2>Theme</h2>
        <p className="muted">One theme for this page and the boot menu. The menu's colours show on graphical (UEFI) consoles.</p>
        <div className="theme">
          <div className="swatches">
            {themeFields.map((f) => (
              <label key={f.key}>
                <input type="color" value={s.theme[f.key]} onInput={(e) => setTheme(f.key, e.currentTarget.value)} /> {f.label}
              </label>
            ))}
          </div>
          <MenuPreview theme={s.theme} />
        </div>
        <div className="buttons">
          <button type="submit">Save</button>
          {saved && <span className="muted">{saved}</span>}
          {error && <span className="error">{error}</span>}
        </div>
      </form>
    </section>
  );
}

/** Roughly what iPXE draws with these colours. */
function MenuPreview({ theme }: { theme: Theme }) {
  const rows: [string, "gap" | "item" | "selected"][] = [
    ["This is bench-pc (12ab34) at 10.255.7.9", "gap"],
    ["No image assigned. Choose one here, or in the web UI.", "gap"],
    ["", "gap"],
    ["Check for an assignment now              (8)", "selected"],
    ["Boot from local disk", "item"],
    ["Tools", "gap"],
    ["netboot.xyz", "item"],
    ["Power and tools", "gap"],
    ["Reboot", "item"],
    ["Power off", "item"],
  ];
  return (
    <div className="menu-preview" style={{ background: theme.background, color: theme.text }}>
      <div className="title">BootBob</div>
      {rows.map(([t, kind], i) => (
        <div
          key={i}
          className="line"
          style={
            kind === "selected"
              ? { background: theme.accent, color: theme.background }
              : kind === "gap"
                ? { color: theme.muted }
                : undefined
          }
        >
          {t || " "}
        </div>
      ))}
    </div>
  );
}
