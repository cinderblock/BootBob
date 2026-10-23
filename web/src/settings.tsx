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
              .filter((i) => i.action !== null || i.id === "menu")
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
