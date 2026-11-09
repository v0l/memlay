import type { ComponentChildren } from "preact";
import { fmt, fmtUs } from "./format";
import type { Hist } from "./types";

export function Card(props: { label: string; value: string; sub?: string }) {
  return (
    <div class="card">
      <div class="k">{props.label}</div>
      <div class="v">{props.value}</div>
      <div class="s">{props.sub}</div>
    </div>
  );
}

export function HistChart(props: { title: string; hist: Hist }) {
  const { hist } = props;
  const max = Math.max(1, ...hist.buckets.map((b) => b.count));
  const edge = hist.buckets[hist.buckets.length - 2]?.le_us ?? null;
  const pct = (label: string, v: number | null) => (
    <span>
      {label} <b>{v == null ? "-" : `<= ${fmtUs(v)}`}</b>
    </span>
  );
  return (
    <div class="hist">
      <h3>
        {props.title} ({fmt(hist.count)} samples)
      </h3>
      <div class="bars">
        {hist.buckets.map((b) => (
          <div
            class="bar"
            style={{ height: `${Math.max(1, (b.count / max) * 100)}%` }}
            title={`${b.le_us == null ? `> ${fmtUs(edge)}` : `<= ${fmtUs(b.le_us)}`}: ${fmt(b.count)}`}
          />
        ))}
      </div>
      <div class="axis">
        <span>{`<= ${fmtUs(hist.buckets[0]?.le_us)}`}</span>
        <span>{`> ${fmtUs(edge)}`}</span>
      </div>
      <div class="pct">
        {pct("p50", hist.p50_us)}
        {pct("p95", hist.p95_us)}
        {pct("p99", hist.p99_us)}
      </div>
    </div>
  );
}

export function CountTag(props: { n: number }) {
  const cls = props.n > 5 ? "hot" : props.n > 1 ? "warm" : "ok";
  return <span class={`tag ${cls}`}>{props.n}</span>;
}

export type Column<T> = {
  head: string;
  num?: boolean;
  mono?: boolean;
  wrap?: boolean;
  cell: (row: T) => ComponentChildren;
};

export function DataTable<T>(props: {
  columns: Column<T>[];
  rows: T[];
  rowKey: (row: T) => string | number;
  detail?: (row: T) => ComponentChildren;
  expanded: string | number | null;
  onToggle: (key: string | number | null) => void;
}) {
  const { columns, rows, rowKey, detail, expanded, onToggle } = props;
  return (
    <div class="tablewrap">
      <table>
        <thead>
          <tr>
            {columns.map((c) => (
              <th class={c.num ? "num" : ""}>{c.head}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => {
            const key = rowKey(row);
            const open = expanded === key;
            return (
              <>
                <tr
                  class={detail ? "row" : ""}
                  onClick={detail ? () => onToggle(open ? null : key) : undefined}
                >
                  {columns.map((c) => (
                    <td class={[c.num && "num", c.mono && "mono", c.wrap && "wrap"].filter(Boolean).join(" ")}>
                      {c.cell(row)}
                    </td>
                  ))}
                </tr>
                {open && detail && (
                  <tr>
                    <td colSpan={columns.length}>{detail(row)}</td>
                  </tr>
                )}
              </>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
