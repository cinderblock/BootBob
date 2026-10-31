import { useEffect, useState } from "react";
import { api, type CacheStatus, type CatalogueEntry, type CatalogueOverview, type CatalogueView, type PiOsImage } from "./api";
import type { Live } from "./live";
import { ago } from "./time";

function size(n: number): string {
  if (n >= 1 << 30) return `${(n / (1 << 30)).toFixed(1)} GiB`;
  return `${Math.ceil(n / (1 << 20))} MB`;
}

function describe(s: CacheStatus): string {
  switch (s.state) {
    case "absent":
      return "Not fetched: the first boot that needs it fetches it";
    case "ready":
      return `Here, ${size(s.size)}, checksum good`;
    case "fetching": {
      if (s.phase === "downloading" && s.total) return `Downloading: ${Math.floor((s.done * 100) / s.total)}% of ${size(s.total)}`;
      return `${s.phase[0].toUpperCase()}${s.phase.slice(1)}: ${size(s.done)}`;
    }
    case "failed":
      return `Failed: ${s.why}${s.permanent ? "" : " (it tries again when a machine asks)"}`;
  }
}

interface Draft {
  id: string;
  title: string;
  store: string;
  url: string;
  checksum: string;
  imageChecksum: string;
  pinned: boolean;
}

const blank: Draft = { id: "", title: "", store: "", url: "", checksum: "", imageChecksum: "", pinned: false };

function toEntry(d: Draft): CatalogueEntry {
  return {
    id: d.id.trim(),
    title: d.title.trim(),
    store: d.store || null,
    url: d.url.trim(),
    checksum: d.checksum.trim() || null,
    image_checksum: d.imageChecksum.trim() || null,
    pinned: d.pinned,
  };
}

/** The name a Raspberry Pi image gets here: the file's, unpacked. */
function diskName(url: string): string {
  const file = url.split("/").pop() ?? "image";
  return file.replace(/\.(xz|gz|zst)$/, "");
}

export function Catalogue({ live, now }: { live: Live; now: number }) {
  const [view, setView] = useState<CatalogueOverview | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [piOs, setPiOs] = useState<PiOsImage[] | null>(null);
  const set = (patch: Partial<Draft>) => setDraft((d) => (d ? { ...d, ...patch } : d));
  const fail = (e: Error) => setError(e.message);
  const load = () => api.catalogue().then(setView, fail);

  // Progress isn't pushed: a download changes too often for the live feed.
  const busy = view?.entries.some((e) => e.status.state === "fetching") ?? false;
  useEffect(() => {
    load();
    const t = setInterval(load, busy ? 2000 : 10000);
    return () => clearInterval(t);
  }, [busy, live.events.length]);

  const users = (e: CatalogueView) => {
    const source = e.store ? `${e.store}:${e.id}` : e.id;
    return live.images.filter(
      (i) => i.pi?.source === source || i.disk?.source === source || (i.action && JSON.stringify(i.action).includes(`{cached:${e.id}}`)),
    );
  };

  return (
    <section>
      <div className="toolbar">
        <h2>Catalogue</h2>
        <button onClick={() => setDraft({ ...blank })}>Add an entry</button>
      </div>
      <p className="muted">
        What BootBob can fetch when a machine needs it: a URL and a checksum. A machine that needs a file that isn't here waits (at the
        menu, or a Pi keeps asking) until it has arrived and matched its checksum, and nothing unverified ever reaches a machine. Each host
        keeps what it fetched up to its cap, dropping the least recently used.
      </p>
      {view && (
        <p className="muted small">
          {view.hosts.map((h) => (
            <span key={h.store ?? "here"} className="host">
              {h.store ?? "This server"}:{" "}
              {h.error ?? `${size(h.used)} of ${size(h.cap)} used${h.free === null ? "" : `, ${size(h.free)} free on the disk`}`}
              {"  "}
            </span>
          ))}
        </p>
      )}
      <table className="images">
        <thead>
          <tr>
            <th>Entry</th>
            <th>Kept on</th>
            <th>State</th>
            <th>Last used</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {view?.entries.map((e) => (
            <tr key={e.id}>
              <td>
                <strong>{e.title}</strong> <span className="muted small">{e.id}</span>
                {users(e).length > 0 && <div className="muted small">Booted by {users(e).map((i) => i.title).join(", ")}</div>}
              </td>
              <td>{e.store ?? "this server"}</td>
              <td className={e.status.state === "failed" ? "error" : "muted"}>
                {describe(e.status)}
                {e.kept && <div className="small">Kept: {e.kept}</div>}
              </td>
              <td className="muted">{e.last_used ? ago(e.last_used, now) : "never"}</td>
              <td className="actions">
                {e.status.state !== "ready" && e.status.state !== "fetching" && (
                  <button className="secondary" onClick={() => api.fetchEntry(e.id).then(load, fail)}>
                    {e.status.state === "failed" ? "Try again" : "Fetch now"}
                  </button>
                )}
                <button
                  className="secondary"
                  title="A pinned file is never evicted"
                  onClick={() => api.putEntry({ ...e, pinned: !e.pinned }).then(load, fail)}
                >
                  {e.pinned ? "Unpin" : "Pin"}
                </button>
                <button className="secondary" onClick={() => api.deleteEntry(e.id).then(load, fail)}>
                  Delete
                </button>
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
            api.putEntry(toEntry(draft)).then(
              () => {
                setDraft(null);
                setError(null);
                load();
              },
              fail,
            );
          }}
        >
          <h3>New catalogue entry</h3>
          <label>
            From Raspberry Pi's list{" "}
            {piOs ? (
              <select
                onChange={(e) => {
                  const p = piOs.find((x) => x.url === e.currentTarget.value);
                  if (p)
                    set({ id: diskName(p.url), title: p.title, url: p.url, checksum: p.checksum ?? "", imageChecksum: p.image_checksum ?? "" });
                }}
              >
                <option value="">Choose one</option>
                {piOs.map((p) => (
                  <option key={p.url} value={p.url}>
                    {p.title} ({p.release_date}, {size(p.download_size)})
                  </option>
                ))}
              </select>
            ) : (
              <button type="button" className="secondary" onClick={() => api.piOsImages().then(setPiOs, fail)}>
                Load the list
              </button>
            )}
          </label>
          <p className="muted small">
            Stock images download and serve their boot files, but have no NBD client in their initramfs, so they can't run their root
            from the network as they are.
          </p>
          <label>
            Id <input required value={draft.id} onInput={(e) => set({ id: e.currentTarget.value })} placeholder="pios-lite.img, or debian-linux" />
          </label>
          <p className="muted small">
            The file's name on its host. Ending in .img makes it a disk image, which is unpacked (xz, gzip or zstd) and can be an image's
            source. Anything else is kept byte for byte, and an iPXE action names it {"{cached:<id>}"}.
          </p>
          <label>
            Title <input required value={draft.title} onInput={(e) => set({ title: e.currentTarget.value })} />
          </label>
          <label>
            Kept on{" "}
            <select value={draft.store} onChange={(e) => set({ store: e.currentTarget.value })}>
              <option value="">This server</option>
              {view?.hosts
                .filter((h) => h.store)
                .map((h) => (
                  <option key={h.store} value={h.store ?? ""}>
                    {h.store}
                  </option>
                ))}
            </select>
          </label>
          <label>
            URL{" "}
            <input
              required
              className="mono"
              value={draft.url}
              onInput={(e) => set({ url: e.currentTarget.value })}
              placeholder="https://…, or store:<store>/<disk>.img for a local copy of a store's disk"
            />
          </label>
          <label>
            Checksum of the download{" "}
            <input className="mono" value={draft.checksum} onInput={(e) => set({ checksum: e.currentTarget.value })} placeholder="sha256:… or sha512:…" />
          </label>
          <label>
            Checksum of the unpacked image{" "}
            <input
              className="mono"
              value={draft.imageChecksum}
              onInput={(e) => set({ imageChecksum: e.currentTarget.value })}
              placeholder="for upstreams that publish only this, like Raspberry Pi"
            />
          </label>
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
