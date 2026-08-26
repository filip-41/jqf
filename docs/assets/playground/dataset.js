/**
 * Deterministic sample-data generator for the playground's speed-test
 * example. Shared by the page and `smoke.mjs`, so both run the exact same
 * dataset — no randomness, same bytes everywhere.
 */

const REGIONS = ['emea', 'apac', 'amer', 'latam'];
const STATUSES = ['active', 'trial', 'churned'];
const PRODUCTS = ['widget', 'gadget', 'gizmo', 'doohickey'];

/**
 * Builds one JSON text of `rows` order records (~80 bytes each), shaped like
 * an export from an order system.
 * @param {number} rows record count
 * @returns {string} the JSON array text
 */
export function bigOrderBook(rows = 100000) {
  const out = new Array(rows);
  for (let i = 0; i < rows; i++) {
    out[i] = {
      id: i,
      region: REGIONS[i % 4],
      status: STATUSES[(i >> 2) % 3],
      product: PRODUCTS[(i >> 3) % 4],
      amount: ((i * 7919) % 50000) / 10,
    };
  }
  return JSON.stringify(out);
}
