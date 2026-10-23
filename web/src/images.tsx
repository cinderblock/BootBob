import { useState } from "react";
import { api, type Action, type Image } from "./api";
import type { Live } from "./live";

const kinds: { kind: Action["kind"]; label: string }[] = [
  { kind: "kernel", label: "Linux kernel + initrd" },
  { kind: "chain", label: "Chain a URL (iPXE script or EFI binary)" },
  { kind: "script", label: "iPXE script" },
];

function describeAction(a: Action | null): string {
  if (!a) return "the fallback menu";
  switch (a.kind) {
    case "local_disk":
      return "back to the firmware (local disk)";
    case "reboot":
      return "reboot";
    case "poweroff":
      return "power off";
    case "shell":
      return "iPXE shell";
    case "chain":
      return `chain ${a.url}`;
    case "kernel":
      return `kernel ${a.kernel}`;
    case "script":
      return `script, ${a.script.split("\n").length} lines`;
  }
}

const blank = (): Draft => ({ id: "", title: "", group: "", kind: "kernel", url: "", kernel: "", initrds: "", cmdline: "", script: "" });

interface Draft {
  id: string;
  title: string;
  group: string;
  kind: Action["kind"];
  url: string;
  kernel: string;
  initrds: string;
  cmdline: string;
  script: string;
}

function toDraft(i: Image): Draft {
  const d = { ...blank(), id: i.id, title: i.title, group: i.group };
  const a = i.action;
  if (a?.kind === "chain") return { ...d, kind: "chain", url: a.url };
  if (a?.kind === "kernel") return { ...d, kind: "kernel", kernel: a.kernel, initrds: a.initrds.join("\n"), cmdline: a.cmdline };
  if (a?.kind === "script") return { ...d, kind: "script", script: a.script };
  return d;
}

function toAction(d: Draft): Action {
  switch (d.kind) {
    case "chain":
      return { kind: "chain", url: d.url.trim() };
    case "script":
      return { kind: "script", script: d.script };
    default:
      return {
        kind: "kernel",
        kernel: d.kernel.trim(),
        initrds: d.initrds
          .split("\n")
          .map((s) => s.trim())
          .filter(Boolean),
        cmdline: d.cmdline.trim(),
      };
  }
}

export function Images({ live }: { live: Live }) {
  const [draft, setDraft] = useState<Draft | null>(null);
  const [error, setError] = useState<string | null>(null);
  const set = (patch: Partial<Draft>) => setDraft((d) => (d ? { ...d, ...patch } : d));

  return (
    <section>
      <div className="toolbar">
        <h2>Images</h2>
        <button onClick={() => setDraft(blank())}>Add an image</button>
      </div>
      <p className="muted">
        Images with a menu section appear in the fallback menu. Built-in images come with BootBob and can't be changed. Installers,
        network roots and backups come in later releases.
      </p>
      <table className="images">
        <thead>
          <tr>
            <th>Image</th>
            <th>Menu section</th>
            <th>Boots</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {live.images.map((i) => (
            <tr key={i.id}>
              <td>
                <strong>{i.title}</strong> <span className="muted small">{i.id}</span>
              </td>
              <td>{i.group || <span className="muted">not listed</span>}</td>
              <td className="muted">{describeAction(i.action)}</td>
              <td className="actions">
                {i.builtin ? (
                  <span className="tag">built in</span>
                ) : (
                  <>
                    <button className="secondary" onClick={() => setDraft(toDraft(i))}>
                      Edit
                    </button>
                    <button
                      className="secondary"
                      onClick={() => api.deleteImage(i.id).then(() => setError(null), (e: Error) => setError(e.message))}
                    >
                      Delete
                    </button>
                  </>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      {error && <p className="error">{error}</p>}

      {draft && (
        <form
          className="editor"
          onSubmit={(e) => {
            e.preventDefault();
            api.putImage({ id: draft.id.trim(), title: draft.title.trim(), group: draft.group.trim(), action: toAction(draft) }).then(
              () => {
                setDraft(null);
                setError(null);
              },
              (err: Error) => setError(err.message),
            );
          }}
        >
          <h3>{live.images.some((i) => i.id === draft.id) ? "Edit image" : "New image"}</h3>
          <label>
            Id <input required value={draft.id} onInput={(e) => set({ id: e.currentTarget.value })} placeholder="debian-installer" />
          </label>
          <label>
            Title <input required value={draft.title} onInput={(e) => set({ title: e.currentTarget.value })} placeholder="Debian installer" />
          </label>
          <label>
            Menu section{" "}
            <input value={draft.group} onInput={(e) => set({ group: e.currentTarget.value })} placeholder="Installers (empty: not in the menu)" />
          </label>
          <label>
            Kind{" "}
            <select value={draft.kind} onChange={(e) => set({ kind: e.currentTarget.value as Action["kind"] })}>
              {kinds.map((k) => (
                <option key={k.kind} value={k.kind}>
                  {k.label}
                </option>
              ))}
            </select>
          </label>
          {draft.kind === "chain" && (
            <label>
              URL <input required value={draft.url} onInput={(e) => set({ url: e.currentTarget.value })} placeholder="https://boot.netboot.xyz" />
            </label>
          )}
          {draft.kind === "kernel" && (
            <>
              <label>
                Kernel URL <input required value={draft.kernel} onInput={(e) => set({ kernel: e.currentTarget.value })} />
              </label>
              <label>
                Initrd URLs (one per line) <textarea rows={2} value={draft.initrds} onInput={(e) => set({ initrds: e.currentTarget.value })} />
              </label>
              <label>
                Kernel command line <input value={draft.cmdline} onInput={(e) => set({ cmdline: e.currentTarget.value })} />
              </label>
            </>
          )}
          {draft.kind === "script" && (
            <label>
              iPXE script <textarea rows={8} className="mono" value={draft.script} onInput={(e) => set({ script: e.currentTarget.value })} />
            </label>
          )}
          <div className="buttons">
            <button type="submit">Save</button>
            <button type="button" className="secondary" onClick={() => setDraft(null)}>
              Cancel
            </button>
          </div>
        </form>
      )}
    </section>
  );
}
