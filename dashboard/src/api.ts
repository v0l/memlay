import type { Report, SortKey } from "./types";

const TOKEN_KEY = "memlay_token";

export class Unauthorized extends Error {}

export const getToken = () => sessionStorage.getItem(TOKEN_KEY);
export const setToken = (t: string) => sessionStorage.setItem(TOKEN_KEY, t);
export const clearToken = () => sessionStorage.removeItem(TOKEN_KEY);

export async function fetchReport(top: number, sort: SortKey): Promise<Report> {
  const token = getToken();
  const res = await fetch(`analytics?top=${top}&sort=${sort}`, {
    headers: token ? { Authorization: `Bearer ${token}` } : {},
  });
  if (res.status === 401) throw new Unauthorized();
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  return res.json();
}
