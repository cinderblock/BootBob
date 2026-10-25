// BootBob's API: types mirror backend/crates/bootbob/src/model.rs and http.rs.

export type Millis = number;

export type Action =
  | { kind: "local_disk" }
  | { kind: "reboot" }
  | { kind: "poweroff" }
  | { kind: "shell" }
  | { kind: "chain"; url: string }
  | { kind: "kernel"; kernel: string; initrds: string[]; cmdline: string }
  | { kind: "script"; script: string };

export interface Assignment {
  image: string;
  once: boolean;
  set_at?: Millis;
}

export interface Device {
  mac: string;
  name: string | null;
  notes: string | null;
  first_seen: Millis;
  last_seen: Millis;
  ip: string | null;
  firmware: string | null;
  http_boot: boolean;
  uuid: string | null;
  serial: string | null;
  manufacturer: string | null;
  product: string | null;
  asset: string | null;
  assignment: Assignment | null;
  last_boot: { at: Millis; image: string; title: string; reason: string } | null;
  menu: { since: Millis; last_poll: Millis; polls: number } | null;
  dhcp: {
    last_discover: Millis;
    arches: number[];
    vendor_class: string | null;
    ipxe: boolean;
    offer: { at: Millis; server: string | null; ip: string | null; boot_server: string | null; boot_file: string | null } | null;
  } | null;
  tftp: { at: Millis; path: string; served: string | null; ok: boolean | null } | null;
  last_script: Millis | null;
  pi_session: { image: string; started: Millis; last: Millis } | null;
  // Computed by the server:
  code: string;
  state: "at_menu" | "menu_idle" | "pi_waiting" | "stuck" | "booted" | "seen";
  summary: string;
  next: { image: string; title: string; reason: string };
}

export interface Image {
  id: string;
  title: string;
  group: string;
  action: Action | null;
  pi: PiBoot | null;
  builtin: boolean;
}

/** A Raspberry Pi boot tree: a directory or disk image in the images directory. */
export interface PiBoot {
  source: string;
  cmdline: string;
  config: string;
  /** Each machine gets its own copy-on-write overlay; the image itself is never written. */
  cow: boolean;
}

/** A machine's copy-on-write overlay of a disk image. */
export interface Overlay {
  source: string;
  allocated: number;
  in_use: boolean;
}

export interface PiSource {
  path: string;
  kind: "directory" | "disk_image";
  size: number | null;
  boot_files: string[];
  error: string | null;
}

export interface Theme {
  background: string;
  surface: string;
  text: string;
  muted: string;
  accent: string;
}

export interface Settings {
  default_image: string;
  pi_default_image: string;
  poll_secs: number;
  menu_timeout_secs: number;
  theme: Theme;
}

export interface BootEvent {
  at: Millis;
  mac: string | null;
  kind: string;
  message: string;
}

export interface Status {
  version: string;
  boot_filename: string;
  tftp: string | null;
  http: string;
  public_url: string | null;
  listener: {
    interface: string | null;
    running: boolean;
    error: string | null;
    dhcp_seen: number;
    pxe_seen: number;
    last_seen: Millis | null;
  };
  ipxe_builds: string[];
  devices: number;
}

async function call<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(path, {
    method,
    headers: body === undefined ? undefined : { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!res.ok) {
    let msg = `${res.status} ${res.statusText}`;
    try {
      msg = (await res.json()).error ?? msg;
    } catch {
      // not JSON
    }
    throw new Error(msg);
  }
  return res.status === 204 ? (undefined as T) : res.json();
}

export const api = {
  status: () => call<Status>("GET", "/api/status"),
  devices: () => call<Device[]>("GET", "/api/devices"),
  patchDevice: (mac: string, patch: { name?: string | null; notes?: string | null }) =>
    call<Device>("PATCH", `/api/devices/${mac}`, patch),
  forgetDevice: (mac: string) => call<void>("DELETE", `/api/devices/${mac}`),
  assign: (mac: string, a: Assignment) => call<Device>("PUT", `/api/devices/${mac}/assignment`, a),
  unassign: (mac: string) => call<Device>("DELETE", `/api/devices/${mac}/assignment`),
  images: () => call<Image[]>("GET", "/api/images"),
  putImage: (img: Omit<Image, "builtin">) => call<Image>("PUT", `/api/images/${encodeURIComponent(img.id)}`, img),
  deleteImage: (id: string) => call<void>("DELETE", `/api/images/${encodeURIComponent(id)}`),
  piSources: () => call<PiSource[]>("GET", "/api/pi/sources"),
  overlays: (mac: string) => call<Overlay[]>("GET", `/api/devices/${mac}/overlays`),
  resetOverlay: (mac: string, source: string) => call<void>("DELETE", `/api/devices/${mac}/overlays/${encodeURIComponent(source)}`),
  settings: () => call<Settings>("GET", "/api/settings"),
  putSettings: (s: Settings) => call<Settings>("PUT", "/api/settings", s),
  events: (mac?: string) => call<BootEvent[]>("GET", `/api/events${mac ? `?mac=${mac}` : ""}`),
};

/** What `/api/stream` sends. */
export type Change =
  | { what: "device"; mac: string }
  | { what: "device_removed"; mac: string }
  | { what: "images" }
  | { what: "settings" }
  | { what: "event" }
  | { what: "resync" };

export interface DoctorCheck {
  id: string;
  title: string;
  status: "ok" | "warn" | "fail" | "skip";
  detail: string;
  fix: string | null;
}

export interface DoctorReport {
  at: Millis;
  checks: DoctorCheck[];
}

export const doctor = {
  last: () => call<DoctorReport | null>("GET", "/api/doctor"),
  run: () => call<DoctorReport>("POST", "/api/doctor"),
};
