import { useEffect, useState } from "react";
import { doctor, type DoctorReport } from "./api";
import { ago, fullTime } from "./time";

const mark = { ok: "✓", warn: "!", fail: "✗", skip: "–" } as const;

export function Doctor({ now }: { now: number }) {
  const [report, setReport] = useState<DoctorReport | null>(null);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    doctor.last().then(setReport, () => {});
  }, []);
  const run = () => {
    setRunning(true);
    doctor
      .run()
      .then(
        (r) => {
          setReport(r);
          setError(null);
        },
        (e: Error) => setError(e.message),
      )
      .finally(() => setRunning(false));
  };
  return (
    <section>
      <div className="toolbar">
        <h2>Network check</h2>
        <button onClick={run} disabled={running}>
          {running ? "Checking…" : "Run the check"}
        </button>
      </div>
      <p className="muted">
        Checks that machines on this network will find BootBob: the iPXE builds, TFTP, the DHCP listener, and what the network's DHCP
        server tells a netbooting BIOS or UEFI machine (BootBob asks it as a pretend client, never taking an address). Problems come with
        the exact fix; BootBob never changes the network itself.
      </p>
      {error && <p className="error">{error}</p>}
      {report ? (
        <>
          <p className="muted small">
            Last run <time title={fullTime(report.at)}>{ago(report.at, now)}</time>
          </p>
          <ul className="checks">
            {report.checks.map((c) => (
              <li key={c.id} className={c.status}>
                <span className="mark">{mark[c.status]}</span>
                <div>
                  <strong>{c.title}</strong>
                  <div>{c.detail}</div>
                  {c.fix && <div className="fix">Fix: {c.fix}</div>}
                </div>
              </li>
            ))}
          </ul>
        </>
      ) : (
        <p>Not run yet.</p>
      )}
    </section>
  );
}
