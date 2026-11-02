import { useEffect, useState } from "react";
import { api, type Device, type PiSource, type Track } from "./api";
import type { Live } from "./live";
import { ago, fullTime } from "./time";

/** Where a following machine stands, in a few words. */
function standing(d: Device, t: Track, now: number): string {
  if (d.moving) return d.moving.back ? `going back to ${d.moving.to}` : `moving to ${d.moving.to}`;
  const running = d.last_boot?.image ?? "nothing yet";
  const quiet = !d.check || now - d.check.at > 30 * 60_000;
  if (running === t.current) return quiet ? "up to date, not checking in" : "up to date";
  if (d.check?.busy) return `on ${running}, busy (${d.check.busy})`;
  return quiet ? `on ${running}, not checking in: boots ${t.current} next time` : `on ${running}, waiting its turn`;
}

export function Rollouts({ live, now }: { live: Live; now: number }) {
  const [error, setError] = useState<string | null>(null);
  // Asks the stores, so only here and only when something changed.
  const [sources, setSources] = useState<PiSource[]>([]);
  useEffect(() => {
    api.piSources().then(setSources, () => setSources([]));
  }, [live.tracks.length, live.tracks.flatMap((t) => t.versions).length]);
  const run = (p: Promise<unknown>) => p.then(() => setError(null), (e: Error) => setError(e.message));
  const s = live.settings;
  return (
    <section>
      <div className="toolbar">
        <h2>Rollouts</h2>
      </div>
      <p className="muted">
        A track is a series of versions of one golden image. Machines that follow one move to its current version one at a time, in the
        daily window ({s ? `${s.rollout_start} ${s.time_zone || s.network_time_zone || "UTC"}, ${s.rollout_minutes} min` : "…"}), stopping at the first that doesn't come
        back healthy. Their overlays are never reset: each version has its own, so going back brings a machine's state back with it.
      </p>
      {error && <p className="error">{error}</p>}
      {live.tracks.map((t) => (
        <TrackCard key={t.id} t={t} live={live} sources={sources} now={now} run={run} />
      ))}
      <NewTrack live={live} run={run} />
    </section>
  );
}

function TrackCard({
  t,
  live,
  sources,
  now,
  run,
}: {
  t: Track;
  live: Live;
  sources: PiSource[];
  now: number;
  run: (p: Promise<unknown>) => void;
}) {
  const [source, setSource] = useState("");
  const taken = new Set(live.images.flatMap((i) => [i.pi?.source, i.disk?.source]));
  const machines = live.devices.filter((d) => d.assignment?.track === t.id);
  const behind = machines.filter((d) => d.last_boot?.image !== t.current).length;
  return (
    <div className="track">
      <h3>
        {t.title} <span className="muted small">{t.id}</span>
      </h3>
      {t.halted && (
        <p className="error">
          Halted {ago(t.halted.at, now)}: {t.halted.why}
        </p>
      )}
      {t.blocked && <p className="error">Can't start: {t.blocked}</p>}
      <p className="muted small">
        {behind === 0 ? "Every machine is on the current version." : `${behind} of ${machines.length} still to move.`}{" "}
        {t.open ? "Moving now." : t.next_window ? `Next window ${fullTime(t.next_window)}.` : ""}
      </p>
      <div className="buttons">
        <button onClick={() => run(api.runTrack(t.id))}>{t.halted ? "Resume now" : "Run now"}</button>
        {!t.halted && (
          <button className="secondary" onClick={() => run(api.haltTrack(t.id))}>
            Stop
          </button>
        )}
      </div>

      <h4>Versions</h4>
      <ul className="overlays">
        {[...t.versions].reverse().map((v) => (
          <li key={v.image}>
            <strong>{v.image}</strong> <time className="muted small" title={fullTime(v.added_at)}>{ago(v.added_at, now)}</time>{" "}
            {v.image === t.current ? (
              <span className="tag">current</span>
            ) : (
              <button
                className="secondary"
                title="Make this the current version and move machines to it now. Each gets back the overlay it had on it."
                onClick={() => run(api.releaseVersion(t.id, v.image, true))}
              >
                Move machines here
              </button>
            )}
          </li>
        ))}
      </ul>
      <form
        className="assign"
        onSubmit={(e) => {
          e.preventDefault();
          if (source) run(api.addVersion(t.id, source).then(() => setSource("")));
        }}
      >
        <select value={source} onChange={(e) => setSource(e.currentTarget.value)}>
          <option value="">New version from a disk…</option>
          {sources
            .filter((s) => s.kind === "disk_image" && !s.error && !taken.has(s.path))
            .map((s) => (
              <option key={s.path} value={s.path}>
                {s.path}
              </option>
            ))}
        </select>
        <button type="submit" disabled={!source}>
          Add for the next window
        </button>
      </form>

      <h4>Machines following it</h4>
      {machines.length === 0 ? (
        <p className="muted small">None. A machine follows a track from its own page under Machines.</p>
      ) : (
        <ul className="overlays">
          {machines.map((d) => (
            <li key={d.mac}>
              <strong>{d.name ?? d.code}</strong> <span className="muted">{standing(d, t, now)}</span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function NewTrack({ live, run }: { live: Live; run: (p: Promise<unknown>) => void }) {
  const [id, setId] = useState("");
  const [title, setTitle] = useState("");
  const [image, setImage] = useState("");
  const candidates = live.images.filter((i) => i.pi?.cow || i.disk?.cow);
  return (
    <form
      className="naming"
      onSubmit={(e) => {
        e.preventDefault();
        run(api.putTrack(id, title, image).then(() => setId("")));
      }}
    >
      <h3>New track</h3>
      <p className="muted small">
        It starts from a copy-on-write image whose command line uses <code>{"{disk}"}</code> and <code>bootbob.url={"{bootbob}"}</code>.
      </p>
      <input placeholder="id" value={id} onInput={(e) => setId(e.currentTarget.value)} />
      <input placeholder="Title" value={title} onInput={(e) => setTitle(e.currentTarget.value)} />
      <select value={image} onChange={(e) => setImage(e.currentTarget.value)}>
        <option value="">First version…</option>
        {candidates.map((i) => (
          <option key={i.id} value={i.id}>
            {i.title}
          </option>
        ))}
      </select>
      <button type="submit" disabled={!id || !title || !image}>
        Create
      </button>
    </form>
  );
}
