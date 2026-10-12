// Lightweight text matching for intents. Good enough to catch "add json output"
// vs "support --json flag for output"; an embedding index (e.g. Vectorize) can
// replace `similarity` later without changing callers.

const STOP = new Set(
  "a an and are as at be by for from has have in into is it its of on or that the this to was were will with when which while should would can could add adds added support supports allow allows make makes new option feature".split(
    " ",
  ),
);

function stem(w: string): string {
  if (w.length > 5 && w.endsWith("ing")) return w.slice(0, -3);
  if (w.length > 4 && w.endsWith("ed")) return w.slice(0, -2);
  if (w.length > 3 && w.endsWith("s") && !w.endsWith("ss")) return w.slice(0, -1);
  return w;
}

export function terms(text: string): string[] {
  const words = text
    .toLowerCase()
    // keep identifiers like --json, foo_bar, Foo.bar intact enough to match
    .split(/[^a-z0-9_]+/)
    .filter((w) => w.length > 1 && !STOP.has(w))
    .map(stem);
  return [...new Set(words)];
}

/** Cosine similarity over binary term vectors, weighted by inverse document frequency. */
export function similarity(a: string[], b: string[], idf: (t: string) => number = () => 1): number {
  if (a.length === 0 || b.length === 0) return 0;
  const bs = new Set(b);
  let dot = 0;
  let na = 0;
  let nb = 0;
  for (const t of a) {
    const w = idf(t) ** 2;
    na += w;
    if (bs.has(t)) dot += w;
  }
  for (const t of b) nb += idf(t) ** 2;
  return dot / Math.sqrt(na * nb);
}
