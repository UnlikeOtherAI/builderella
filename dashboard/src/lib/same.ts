/**
 * Do two JSON values say the same thing? Key order is ignored and an absent
 * key equals `null`, as it does for an optional field on the wire.
 */
export function sameJson(a: unknown, b: unknown): boolean {
  return canonical(a) === canonical(b);
}

function canonical(value: unknown): string {
  return JSON.stringify(normalize(value)) ?? 'null';
}

function normalize(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(normalize);
  if (value !== null && typeof value === 'object') {
    const out: Record<string, unknown> = {};
    for (const key of Object.keys(value).sort()) {
      const child = (value as Record<string, unknown>)[key];
      if (child !== null && child !== undefined) out[key] = normalize(child);
    }
    return out;
  }
  return value;
}
