import { useCallback, useEffect, useState } from "preact/hooks";
import { Unauthorized, clearToken, fetchReport, getToken, setToken } from "./api";
import { fmt, fmtDur, fmtMs, fmtUs } from "./format";
import {
  clientColumns,
  clientDetail,
  connColumns,
  ipColumns,
  ipDetail,
  reqColumns,
  reqDetail,
  shapeColumns,
} from "./tables";
import type { Report, SortKey } from "./types";
import { Card, DataTable, HistChart } from "./ui";

type Tab = "reqs" | "shapes" | "clients" | "ips" | "conns";

const TABS: [Tab, string, (r: Report) => number][] = [
  ["reqs", "REQs", (r) => r.top_reqs.length],
  ["shapes", "REQ shapes", (r) => r.top_shapes.length],
  ["clients", "Clients", (r) => r.top_clients.length],
  ["ips", "IPs", (r) => r.top_ips.length],
  ["conns", "Connections", (r) => r.top_connections.length],
];

function Unlock(props: { onSave: (token: string) => void }) {
  const [value, setValue] = useState("");
  return (
    <div>
      <p class="err">This relay requires an analytics token.</p>
      <input
        type="password"
        placeholder="analytics_token"
        autocomplete="off"
        value={value}
        onInput={(e) => setValue(e.currentTarget.value)}
        onKeyDown={(e) => e.key === "Enter" && props.onSave(value)}
      />
      <button onClick={() => props.onSave(value)}>Unlock</button>
    </div>
  );
}

function Overview(props: { report: Report }) {
  const d = props.report;
  return (
    <>
      <div class="cards">
        <Card label="Uptime" value={fmtDur(d.uptime_secs)} />
        <Card label="Connections" value={fmt(d.connections.active)} sub={`${fmt(d.connections.total)} total`} />
        <Card
          label="REQs"
          value={fmt(d.req.started)}
          sub={`${fmtUs(d.req.cpu_avg_us)} cpu avg, ${fmtUs(d.req.wall_avg_us)} resp avg`}
        />
        <Card
          label="Events published"
          value={fmt(d.publish.started)}
          sub={`${fmtUs(d.publish.cpu_avg_us)} cpu avg, ${fmtUs(d.publish.wall_avg_us)} resp avg`}
        />
        <Card label="REQ CPU total" value={fmtMs(d.req.cpu_total_ms)} sub={`${fmt(d.req.items)} events sent`} />
        <Card label="Publish CPU total" value={fmtMs(d.publish.cpu_total_ms)} sub={`${fmt(d.publish.items)} accepted`} />
        <Card
          label="Tracked"
          value={`${fmt(d.tracked.reqs)} REQs`}
          sub={`${fmt(d.tracked.clients)} clients, ${fmt(d.tracked.ips)} IPs (cap ${fmt(d.tracked.max_entries)})`}
        />
      </div>
      <h2>Latency histograms (all requests)</h2>
      <div class="hists">
        <HistChart title="REQ CPU time" hist={d.req.cpu_hist} />
        <HistChart title="REQ response time" hist={d.req.wall_hist} />
        <HistChart title="EVENT CPU time" hist={d.publish.cpu_hist} />
        <HistChart title="EVENT response time" hist={d.publish.wall_hist} />
      </div>
    </>
  );
}

function Rows(props: { tab: Tab; report: Report }) {
  const [expanded, setExpanded] = useState<string | number | null>(null);
  const common = { expanded, onToggle: setExpanded };
  const r = props.report;
  switch (props.tab) {
    case "reqs":
      return <DataTable {...common} columns={reqColumns} rows={r.top_reqs} rowKey={(x) => x.key} detail={reqDetail} />;
    case "shapes":
      return <DataTable {...common} columns={shapeColumns} rows={r.top_shapes} rowKey={(x) => x.shape} />;
    case "clients":
      return (
        <DataTable {...common} columns={clientColumns} rows={r.top_clients} rowKey={(x) => x.fingerprint} detail={clientDetail} />
      );
    case "ips":
      return <DataTable {...common} columns={ipColumns} rows={r.top_ips} rowKey={(x) => x.ip} detail={ipDetail} />;
    case "conns":
      return <DataTable {...common} columns={connColumns} rows={r.top_connections} rowKey={(x) => x.id} />;
  }
}

export function App() {
  const [report, setReport] = useState<Report | null>(null);
  const [locked, setLocked] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [updated, setUpdated] = useState<Date | null>(null);
  const [sort, setSort] = useState<SortKey>("cpu");
  const [top, setTop] = useState(25);
  const [every, setEvery] = useState(5000);
  const [tab, setTab] = useState<Tab>("reqs");
  const [hasToken, setHasToken] = useState(!!getToken());

  const load = useCallback(async () => {
    try {
      setReport(await fetchReport(top, sort));
      setLocked(false);
      setError(null);
      setUpdated(new Date());
    } catch (e) {
      if (e instanceof Unauthorized) {
        setReport(null);
        setLocked(true);
        setError(null);
      } else {
        setError((e as Error).message);
      }
    }
  }, [top, sort]);

  useEffect(() => {
    load();
    if (!every) return;
    const t = setInterval(load, every);
    return () => clearInterval(t);
  }, [load, every]);

  const unlock = (token: string) => {
    setToken(token);
    setHasToken(true);
    load();
  };

  const forget = () => {
    clearToken();
    setHasToken(false);
    setReport(null);
    setLocked(true);
  };

  return (
    <>
      <header>
        <h1>memlay admin</h1>
        <label>
          Sort{" "}
          <select value={sort} onChange={(e) => setSort(e.currentTarget.value as SortKey)}>
            <option value="cpu">CPU time</option>
            <option value="wall">Response time</option>
            <option value="count">Request count</option>
            <option value="events">Events</option>
          </select>
        </label>
        <label>
          Rows{" "}
          <select value={top} onChange={(e) => setTop(+e.currentTarget.value)}>
            {[10, 25, 50, 100].map((n) => (
              <option value={n}>{n}</option>
            ))}
          </select>
        </label>
        <label>
          Refresh{" "}
          <select value={every} onChange={(e) => setEvery(+e.currentTarget.value)}>
            <option value={0}>off</option>
            <option value={2000}>2s</option>
            <option value={5000}>5s</option>
            <option value={15000}>15s</option>
          </select>
        </label>
        <button onClick={load}>Refresh now</button>
        {hasToken && <button onClick={forget}>Forget token</button>}
        <span id="status" class={error ? "err" : ""}>
          {error ? `error: ${error}` : updated ? `updated ${updated.toLocaleTimeString()}` : ""}
        </span>
      </header>
      {locked && <Unlock onSave={unlock} />}
      {report && (
        <main>
          <Overview report={report} />
          <div class="tabs">
            {TABS.map(([key, label, count]) => (
              <button class={key === tab ? "on" : ""} onClick={() => setTab(key)}>
                {label} ({count(report)})
              </button>
            ))}
          </div>
          <Rows key={tab} tab={tab} report={report} />
        </main>
      )}
    </>
  );
}
