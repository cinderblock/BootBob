import { useEffect, useState } from "react";
import { api, type Action, type Image, type PiBoot, type PiSource } from "./api";
import type { Live } from "./live";

type Kind = Action["kind"] | "pi";

const kinds: { kind: Kind; label: string }[] = [
  { kind: "kernel", label: "Linux kernel + initrd" },
  { kind: "chain", label: "Chain a URL (iPXE script or EFI binary)" },
  { kind: "script", label: "iPXE script" },
  { kind: "pi", label: "Raspberry Pi boot files" },
];

function describeImage(i: Image): string {
  const pi = i.pi ? `Pi boot files from ${i.pi.source}` : null;
  if (!i.action) return pi ?? "the fallback menu";
  return pi ? `${describeAction(i.action)}; ${pi}` : describeAction(i.action);
}

function describeAction(a: Action): string {
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

const blank = (): Draft => ({
  id: "",
  title: "",
  group: "",
  kind: "kernel",
  url: "",
  kernel: "",
  initrds: "",
  cmdline: "",
  script: "",
  source: "",
  piCmdline: "",
  piConfig: "",
  piCow: false,
  keepAction: null,
  keepPi: null,
});

interface Draft {
  id: string;
  title: string;
  group: string;
  kind: Kind;
  url: string;
  kernel: string;
  initrds: string;
  cmdline: string;
  script: string;
  source: string;
  piCmdline: string;
  piConfig: string;
  piCow: boolean;
  /** The half the editor isn't showing, kept as it was: an image can have both. */
  keepAction: Action | null;
  keepPi: PiBoot | null;
}

function toDraft(i: Image): Draft {
  const d = { ...blank(), id: i.id, title: i.title, group: i.group, keepAction: i.action, keepPi: i.pi };
  if (i.pi && !i.action) return { ...d, kind: "pi", source: i.pi.source, piCmdline: i.pi.cmdline, piConfig: i.pi.config, piCow: i.pi.cow };
  const a = i.action;
  if (a?.kind === "chain") return { ...d, kind: "chain", url: a.url };
  if (a?.kind === "kernel") return { ...d, kind: "kernel", kernel: a.kernel, initrds: a.initrds.join("\n"), cmdline: a.cmdline };
  if (a?.kind === "script") return { ...d, kind: "script", script: a.script };
  return d;
}

/** The image's `action` and `pi` from the editor. */
function toBoot(d: Draft): { action: Action | null; pi: PiBoot | null } {
  if (d.kind === "pi") {
    return { action: d.keepAction, pi: { source: d.source.trim(), cmdline: d.piCmdline.trim(), config: d.piConfig.trim(), cow: d.piCow } };
  }
  return { action: toAction(d), pi: d.keepPi };
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
  const [sources, setSources] = useState<PiSource[]>([]);
  const set = (patch: Partial<Draft>) => setDraft((d) => (d ? { ...d, ...patch } : d));
  const editingPi = draft?.kind === "pi";
  useEffect(() => {
    if (editingPi) api.piSources().then(setSources, () => setSources([]));
  }, [editingPi]);
  const chosen = sources.find((s) => s.path === draft?.source.trim());

  return (
    <section>
      <div className="toolbar">
        <h2>Images</h2>
        <button onClick={() => setDraft(blank())}>Add an image</button>
      </div>
      <p className="muted">
        Images with a menu section appear in the fallback menu. Built-in images come with BootBob and can't be changed. Raspberry Pi
        images serve a boot tree over TFTP to the Pis assigned them. Installers, network roots and backups come in later releases.
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
              <td className="muted">{describeImage(i)}</td>
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
            api.putImage({ id: draft.id.trim(), title: draft.title.trim(), group: draft.group.trim(), ...toBoot(draft) }).then(
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
            <select value={draft.kind} onChange={(e) => set({ kind: e.currentTarget.value as Kind })}>
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
          {draft.kind === "pi" && (
            <>
              <label>
                Boot files{" "}
                <input
                  required
                  list="pi-sources"
                  value={draft.source}
                  onInput={(e) => set({ source: e.currentTarget.value })}
                  placeholder="a directory or .img in the images directory"
                />
                <datalist id="pi-sources">
                  {sources.map((s) => (
                    <option key={s.path} value={s.path}>
                      {s.kind === "disk_image" ? "disk image" : "directory"}
                      {s.boot_files.length ? `: ${s.boot_files.join(", ")}` : ""}
                    </option>
                  ))}
                </datalist>
              </label>
              <p className="muted small">
                {chosen
                  ? chosen.error
                    ? `Can't read it: ${chosen.error}`
                    : chosen.boot_files.length
                      ? `Has ${chosen.boot_files.join(", ")}.`
                      : "No Pi boot files found in it."
                  : sources.length
                    ? `In the images directory: ${sources.map((s) => s.path).join(", ")}.`
                    : "Nothing in the images directory yet. Put a boot tree (a directory) or a decompressed Pi OS .img there."}{" "}
                A disk image's FAT boot partition is read in place.
              </p>
              <label>
                Replace cmdline.txt (empty: the tree's own){" "}
                <input
                  className="mono"
                  value={draft.piCmdline}
                  onInput={(e) => set({ piCmdline: e.currentTarget.value })}
                  placeholder="console=tty1 root=/dev/nbd0p2 …"
                />
              </label>
              <label>
                Add to config.txt <textarea rows={2} className="mono" value={draft.piConfig} onInput={(e) => set({ piConfig: e.currentTarget.value })} />
              </label>
              <p className="muted small">
                Both can use {"{serial}"}, {"{mac}"}, {"{ip}"} and {"{server}"}, filled in for each Pi.
              </p>
              <label title="One golden image for any number of Pis: each writes to its own overlay, and the image stays pristine. Needs a disk image (.img), served over NBD.">
                <input type="checkbox" checked={draft.piCow} onChange={(e) => set({ piCow: e.currentTarget.checked })} /> Copy-on-write: each
                machine gets its own overlay, the image is never written
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
