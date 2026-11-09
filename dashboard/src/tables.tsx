import { fmt, fmtBytes, fmtDur, fmtMs, fmtUs } from "./format";
import { CountTag, type Column } from "./ui";
import type { ClientRow, ConnRow, IpRow, ReqRow, ShapeRow, Sources, Usage } from "./types";

const p99 = (h: { p99_us: number | null }) => (h.p99_us == null ? "-" : `<= ${fmtUs(h.p99_us)}`);

function usageColumns<T>(get: (row: T) => Usage): Column<T>[] {
  return [
    { head: "Count", num: true, cell: (r) => fmt(get(r).started) },
    { head: "Events", num: true, cell: (r) => fmt(get(r).items) },
    { head: "CPU total", num: true, cell: (r) => fmtMs(get(r).cpu_total_ms) },
    { head: "CPU avg", num: true, cell: (r) => fmtUs(get(r).cpu_avg_us) },
    { head: "CPU p99", num: true, cell: (r) => p99(get(r).cpu_hist) },
    { head: "Resp avg", num: true, cell: (r) => fmtUs(get(r).wall_avg_us) },
    { head: "Resp p99", num: true, cell: (r) => p99(get(r).wall_hist) },
    { head: "Resp max", num: true, cell: (r) => fmtUs(get(r).wall_max_us) },
  ];
}

export const sourcesText = (s: Sources) =>
  `IPs (${s.ips.length}): ${s.ips.join(", ") || "-"}\nFingerprints (${s.fingerprints.length}): ${s.fingerprints.join(", ") || "-"}`;

const publishText = (u: Usage) =>
  `Publish: ${fmt(u.started)} events, ${fmtMs(u.cpu_total_ms)} cpu, ${fmtUs(u.wall_avg_us)} resp avg`;

export const reqColumns: Column<ReqRow>[] = [
  { head: "Key", mono: true, cell: (r) => r.key },
  { head: "Shape", wrap: true, cell: (r) => r.shape },
  { head: "IPs", cell: (r) => <CountTag n={r.sources.ips.length} /> },
  { head: "Clients", cell: (r) => <CountTag n={r.sources.fingerprints.length} /> },
  ...usageColumns<ReqRow>((r) => r),
];

export const shapeColumns: Column<ShapeRow>[] = [
  { head: "Shape", wrap: true, cell: (r) => r.shape },
  { head: "Distinct REQs", num: true, cell: (r) => fmt(r.distinct_reqs) },
  { head: "Started", num: true, cell: (r) => fmt(r.started) },
  { head: "Events", num: true, cell: (r) => fmt(r.events) },
  { head: "CPU total", num: true, cell: (r) => fmtMs(r.cpu_total_ms) },
  { head: "Resp total", num: true, cell: (r) => fmtMs(r.wall_total_ms) },
];

export const clientColumns: Column<ClientRow>[] = [
  { head: "Fingerprint", mono: true, cell: (r) => r.fingerprint },
  { head: "User agent", wrap: true, cell: (r) => r.user_agent || "-" },
  { head: "Origin", wrap: true, cell: (r) => r.origin || "-" },
  { head: "Conns", num: true, cell: (r) => fmt(r.connections) },
  { head: "Active", num: true, cell: (r) => fmt(r.active) },
  { head: "IPs", cell: (r) => <CountTag n={r.sources.ips.length} /> },
  { head: "In", num: true, cell: (r) => fmtBytes(r.bytes_in) },
  { head: "Rej", num: true, cell: (r) => fmt(r.rejected) },
  ...usageColumns<ClientRow>((r) => r.req),
];

export const ipColumns: Column<IpRow>[] = [
  { head: "IP", mono: true, cell: (r) => r.ip },
  { head: "Conns", num: true, cell: (r) => fmt(r.connections) },
  { head: "Active", num: true, cell: (r) => fmt(r.active) },
  { head: "Clients", cell: (r) => <CountTag n={r.sources.fingerprints.length} /> },
  { head: "In", num: true, cell: (r) => fmtBytes(r.bytes_in) },
  { head: "Rej", num: true, cell: (r) => fmt(r.rejected) },
  ...usageColumns<IpRow>((r) => r.req),
];

export const connColumns: Column<ConnRow>[] = [
  { head: "ID", cell: (r) => r.id },
  { head: "IP", mono: true, cell: (r) => r.ip },
  { head: "Fingerprint", mono: true, cell: (r) => r.fingerprint },
  { head: "Age", num: true, cell: (r) => fmtDur(r.connected_secs) },
  { head: "In", num: true, cell: (r) => fmtBytes(r.bytes_in) },
  { head: "Rej", num: true, cell: (r) => fmt(r.rejected) },
  ...usageColumns<ConnRow>((r) => r.req),
];

export const clientDetail = (r: ClientRow) => (
  <pre class="detail">{`Headers: ${r.headers}\n${sourcesText(r.sources)}\n\n${publishText(r.publish)}`}</pre>
);

export const ipDetail = (r: IpRow) => (
  <pre class="detail">{`${sourcesText(r.sources)}\n\n${publishText(r.publish)}`}</pre>
);

export const reqDetail = (r: ReqRow) => (
  <>
    <pre class="detail">{JSON.stringify(r.filters, null, 2)}</pre>
    <pre class="detail">{sourcesText(r.sources)}</pre>
  </>
);
