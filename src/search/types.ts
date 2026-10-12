// The search contract. Every operation is scoped to one project (the
// partition key) and is either a batch of exact key lookups or a top-k
// nearest-neighbour query. Nothing needs a scan or a cross-partition join,
// so each operation maps directly onto a sharded key-value store plus a
// namespaced vector or full-text engine. See docs/SEARCH.md.
//
// The index is derived data. The system of record (patches, intents,
// adoptions) can rebuild it at any time, which keeps backends swappable.

export interface IntentDoc {
  id: string;
  projectId: string;
  text: string;
}

export interface PatchDoc {
  id: string;
  projectId: string;
  /** Exact hunk fingerprints (see core/diff.ts). */
  hunks: { path: string; fingerprint: string }[];
  /** MinHash signature and LSH band keys (see core/sketch.ts); absent for empty diffs. */
  sketch: { signature: string; bands: string[] } | null;
}

export interface IntentHit {
  id: string;
  /** Relevance in [0, 1]. Backends may use lexical, semantic or hybrid scoring. */
  relevance: number;
}

export interface HunkHit {
  patchId: string;
  shared: number; // fingerprints in common with the query
  total: number; // fingerprints in the hit patch
}

export interface SketchHit {
  patchId: string;
  signature: string;
}

export interface SearchIndex {
  indexIntent(doc: IntentDoc): Promise<void>;
  indexPatch(doc: PatchDoc): Promise<void>;

  searchIntents(projectId: string, text: string, k: number): Promise<IntentHit[]>;
  patchesByHunks(projectId: string, fingerprints: string[], k: number): Promise<HunkHit[]>;
  /** Candidates sharing at least one LSH band. The caller verifies them by signature. */
  patchesByBands(projectId: string, bands: string[], k: number): Promise<SketchHit[]>;
}
