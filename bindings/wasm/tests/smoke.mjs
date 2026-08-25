// Node smoke test for the jqf wasm binding + JS wrapper. Run through `make bindings-wasm` (which builds the bundle
// first).
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { loadJqf, FLAGS } from '../jqf.js';
// Node cannot fetch() file URLs, so hand the glue the bytes directly.
const wasmBytes = readFileSync(
  fileURLToPath(new URL('../jqf_wasm_bg.wasm', import.meta.url)),
);

const jqf = await loadJqf(wasmBytes);

console.log('version:', jqf.version());

let failures = 0;
function check(name, cond, extra = '') {
  if (!cond) {
    failures++;
    console.log('FAIL:', name, extra);
  } else {
    console.log('ok:', name);
  }
}

let r = jqf.run('.user.name', '{"user":{"name":"Filip"}}');
check('basic json', r.ok === true && r.output.trim() === '"Filip"', JSON.stringify(r));

r = jqf.run('.[] | . * 2', '[1,2,3]');
check('multi output', r.ok && r.output === '2\n4\n6\n', JSON.stringify(r));

r = jqf.run('.a ++', '{"a":1}');
check('compile error', r.ok === false && (r.error || '').length > 0, JSON.stringify(r).slice(0, 200));

r = jqf.run('1/0', 'null');
check('runtime error', r.ok === false, JSON.stringify(r).slice(0, 200));

r = jqf.run('try (1/0) catch "caught"', 'null');
check('try catch', r.ok && r.output.trim() === '"caught"', JSON.stringify(r));

r = jqf.run('.name', 'name: app\nport: 8080\n', { input: 'yaml' });
check('yaml->json', r.ok && r.output.trim() === '"app"', JSON.stringify(r).slice(0, 300));

r = jqf.run('.owner.name', '[owner]\nname = "Tom"\n', { input: 'toml' });
check('toml->json', r.ok && r.output.trim() === '"Tom"', JSON.stringify(r).slice(0, 300));

r = jqf.run('.', 'name,score\na,42\nb,7\n', { input: 'csv-header' });
check('csv header input', r.ok && r.output.trim() === '{"name":"a","score":"42"}\n{"name":"b","score":"7"}', JSON.stringify(r).slice(0, 300));

r = jqf.run('.[1] | tonumber', 'name,score\na,42\nb,7\n', { input: 'csv' });
check('csv array rows', r.ok && r.output === '42\n7\n', JSON.stringify(r).slice(0, 300));

r = jqf.run('.title', '<book><title>Hi</title></book>', { input: 'xml' });
check('xml->json (root is book)', r.ok && r.output.trim() === '["Hi"]', JSON.stringify(r).slice(0, 400));

r = jqf.run('[.. | strings]', '<div><p>a</p><p>b</p></div>', { input: 'html' });
check('html->json', r.ok, JSON.stringify(r).slice(0, 300));

r = jqf.run('.', '{"a":[1,2]}', { indent: 2 });
check('pretty indent', r.ok && r.output.includes('\n  '), JSON.stringify(r));

r = jqf.run('.', '{"a":[1,2]}', { indent: -1 });
check('tab indent (jq --indent -1 law)', r.ok && r.output.includes('\n\t'), JSON.stringify(r));

r = jqf.run('.', '{}', { indent: 12 });
check('indent out of range refused, not clamped', r.ok === false && r.output === '', JSON.stringify(r).slice(0, 200));

r = jqf.run('.greeting', '{"greeting":"hello"}', { flags: FLAGS.RAW_STRINGS });
check('raw flag', r.ok && r.output.trim() === 'hello', JSON.stringify(r));

r = jqf.run('. + 1', '1\n2\n3\n', { input: 'ndjson' });
check('ndjson stream', r.ok && r.output === '2\n3\n4\n', JSON.stringify(r));

r = jqf.run('if .age > 30 then .name else empty end', '{"name":"a","age":31}\n{"name":"b","age":25}\n', { input: 'ndjson' });
check('ndjson per-record filter', r.ok && r.output.trim() === '"a"', JSON.stringify(r).slice(0, 300));

r = jqf.run('.', '1\n2\n3\n', { slurp: true });
check('slurp collects into one array', r.ok && r.output.trim() === '[1,2,3]', JSON.stringify(r));

r = jqf.run('length', '1\n2\n3\n', { input: 'ndjson', slurp: true });
check('record slurp runs once over the array', r.ok && r.output.trim() === '3', JSON.stringify(r));

r = jqf.run('"hi"', null);
check('null input via null', r.ok && r.output.trim() === '"hi"', JSON.stringify(r));

r = jqf.run('[inputs] | length', '1 2 3');
check('inputs family (jq law: first value is dot)', r.ok && r.output.trim() === '2', JSON.stringify(r));

r = jqf.run('select(.age > 30) | .name', '{"name":"a","age":31}\n{"name":"b","age":25}\n', { input: 'ndjson' });
check('ndjson per-record select', r.ok && r.output.trim() === '"a"', JSON.stringify(r).slice(0, 300));

const formats = jqf.formats();
check('formats list', Array.isArray(formats) && formats.some((f) => f.name === 'yaml'));

let deep = '['.repeat(5000) + ']'.repeat(5000);
r = jqf.run('.', deep);
check('deep nesting refused', r.ok === false, JSON.stringify(r).slice(0, 150));

r = jqf.run('def f: f; f', 'null');
check('infinite recursion refused', r.ok === false, JSON.stringify(r).slice(0, 150));

r = jqf.run('100000000000000000000 + 1', 'null');
check('bigint exact', r.ok && r.output.trim() === '100000000000000000001', JSON.stringify(r));

r = jqf.run('test("a+b"; "i")', '"aabbb"');
check('regex', r.ok && r.output.trim() === 'true', JSON.stringify(r));

r = jqf.run('"\\(.a) items"', '{"a":5}');
check('interpolation', r.ok && r.output.trim() === '"5 items"', JSON.stringify(r));

r = jqf.run('.port | .@comment', '# top comment\nname: app\nport: 8080 # inline comment\n', { input: 'yaml' });
check('comment fact read', r.ok, JSON.stringify(r).slice(0, 400));

r = jqf.run('.', '{"a":1,"b":"x"}', { output: 'toml' });
check('json->toml out', r.ok && r.output.includes('a = 1'), JSON.stringify(r).slice(0, 400));

r = jqf.run('.', '{}', { output: 'cbor' });
check('binary out flagged', r.binary === true && (r.output_base64 || '').length > 0, JSON.stringify(r).slice(0, 200));

r = jqf.run('$ENV | type', 'null');
check('env object exists', r.ok && r.output.trim() === '"object"', JSON.stringify(r));

// --- record inputs publishing yaml (the RouteCapability::Record lift) ------

r = jqf.run('.', '{"a":1}\n{"a":2}\n', { input: 'ndjson', output: 'yaml', indent: 0 });
check(
  'ndjson->yaml block separators',
  r.ok && r.output === 'a: 1\n---\na: 2\n',
  JSON.stringify(r).slice(0, 200),
);

r = jqf.run('.', '{"id":1,"name":"widget"}\n{"id":2,"name":"gadget"}\n', { input: 'ndjson', output: 'csv' });
check('ndjson->csv rows', r.ok && r.output === '1,widget\r\n2,gadget\r\n', JSON.stringify(r).slice(0, 200));

console.log(failures === 0 ? '\nALL SMOKE TESTS PASSED' : `\n${failures} FAILURES`);
process.exit(failures === 0 ? 0 : 1);
