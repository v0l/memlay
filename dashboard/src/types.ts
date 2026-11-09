export type Bucket = { le_us: number | null; count: number };

export type Hist = {
  count: number;
  p50_us: number | null;
  p95_us: number | null;
  p99_us: number | null;
  buckets: Bucket[];
};

export type Usage = {
  started: number;
  completed: number;
  items: number;
  cpu_total_ms: number;
  wall_total_ms: number;
  cpu_avg_us: number;
  wall_avg_us: number;
  wall_max_us: number;
  last_seen_secs_ago: number;
  cpu_hist: Hist;
  wall_hist: Hist;
};

export type Sources = { ips: string[]; fingerprints: string[] };

export type ReqRow = Usage & {
  key: string;
  shape: string;
  filters: unknown;
  sources: Sources;
};

export type ShapeRow = {
  shape: string;
  distinct_reqs: number;
  started: number;
  events: number;
  cpu_total_ms: number;
  wall_total_ms: number;
};

export type AccountBase = {
  connections: number;
  active: number;
  bytes_in: number;
  rejected: number;
  req: Usage;
  publish: Usage;
  sources: Sources;
};

export type ClientRow = AccountBase & {
  fingerprint: string;
  user_agent: string;
  origin: string;
  headers: string;
};

export type IpRow = AccountBase & { ip: string };

export type ConnRow = {
  id: number;
  ip: string;
  fingerprint: string;
  connected_secs: number;
  bytes_in: number;
  rejected: number;
  req: Usage;
  publish: Usage;
};

export type Report = {
  uptime_secs: number;
  sort: string;
  connections: { active: number; total: number };
  tracked: { reqs: number; clients: number; ips: number; max_entries: number };
  req: Usage;
  publish: Usage;
  top_reqs: ReqRow[];
  top_shapes: ShapeRow[];
  top_clients: ClientRow[];
  top_ips: IpRow[];
  top_connections: ConnRow[];
};

export type SortKey = "cpu" | "wall" | "count" | "events";
