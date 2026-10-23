// Live data: everything the UI shows, refetched when /api/stream says it changed.

import { useEffect, useRef, useState } from "react";
import { api, type BootEvent, type Change, type Device, type Image, type Settings, type Status } from "./api";

export interface Live {
  status: Status | null;
  devices: Device[];
  images: Image[];
  settings: Settings | null;
  events: BootEvent[];
  error: string | null;
  connected: boolean;
}

type Part = "status" | "devices" | "images" | "settings" | "events";

const loaders: Record<Part, () => Promise<unknown>> = {
  status: api.status,
  devices: api.devices,
  images: api.images,
  settings: api.settings,
  events: () => api.events(),
};

function partsFor(c: Change): Part[] {
  switch (c.what) {
    case "device":
    case "device_removed":
      return ["devices", "status"];
    case "images":
      return ["images", "devices"];
    case "settings":
      return ["settings", "devices"];
    case "event":
      return ["events"];
    case "resync":
      return ["status", "devices", "images", "settings", "events"];
  }
}

export function useLive(): Live {
  const [live, setLive] = useState<Live>({
    status: null,
    devices: [],
    images: [],
    settings: null,
    events: [],
    error: null,
    connected: false,
  });
  const pending = useRef(new Set<Part>());
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    const load = (parts: Iterable<Part>) => {
      for (const p of parts) {
        loaders[p]()
          .then((v) => setLive((l) => ({ ...l, [p]: v, error: null })))
          .catch((e: Error) => setLive((l) => ({ ...l, error: e.message })));
      }
    };
    // Bursts of changes (a machine polling, booting, logging) become one refetch.
    const schedule = (parts: Part[]) => {
      parts.forEach((p) => pending.current.add(p));
      if (timer.current) return;
      timer.current = setTimeout(() => {
        timer.current = null;
        const ps = [...pending.current];
        pending.current.clear();
        load(ps);
      }, 150);
    };
    load(Object.keys(loaders) as Part[]);
    const es = new EventSource("/api/stream");
    es.onopen = () => {
      setLive((l) => ({ ...l, connected: true }));
      schedule(["status", "devices", "images", "settings", "events"]);
    };
    es.onerror = () => setLive((l) => ({ ...l, connected: false }));
    es.onmessage = (m) => {
      try {
        schedule(partsFor(JSON.parse(m.data) as Change));
      } catch {
        // ignore malformed
      }
    };
    return () => {
      es.close();
      if (timer.current) clearTimeout(timer.current);
    };
  }, []);
  return live;
}

/** The current time, ticking every second, for "3 s ago". */
export function useNow(): number {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, []);
  return now;
}
