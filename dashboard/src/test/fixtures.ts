import raw from '../mocks/fixtures.json';

// Test code edits copies of the fixture document to make broken or
// unfamiliar responses, so it is handled as untyped JSON on purpose.
// eslint-disable-next-line @typescript-eslint/no-explicit-any
export type Json = any;

/** A fresh, mutable copy of the fixture document as the server would send it. */
export function rawFixtures(): Json {
  return structuredClone(raw) as Json;
}
