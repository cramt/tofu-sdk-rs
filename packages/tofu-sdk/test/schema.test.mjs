// Unit tests for the Zod → cty schema derivation (`ts/schema.ts`, compiled to
// `dist/schema.js`). Pure — no native addon, no engine — so it runs after a plain
// `tsc` build. Covers the "algebraic types define everything" direction: cty +
// dispositions read straight off the Zod type (`z.set`, per-field `.meta()`), and
// the JS `Set` ⇄ JSON-array marshaling that keeps `z.set` honest over the wire.

import assert from "node:assert/strict";
import { test } from "node:test";
import { z } from "zod";

import {
  ctyFromZod,
  functionSignatureJson,
  paramNames,
  reviveSets,
  schemaJson,
  searchKeysOf,
  toWireJson,
  transformFields,
} from "../dist/schema.js";

const attrs = (schema, dispositions) => JSON.parse(schemaJson(schema, dispositions)).attributes;
const blocks = (schema, dispositions) => JSON.parse(schemaJson(schema, dispositions)).blocks;
const byName = (list, name) => list.find((a) => a.name === name);

test("scalars derive to their cty types", () => {
  assert.equal(ctyFromZod(z.string()), "string");
  assert.equal(ctyFromZod(z.number()), "number");
  assert.equal(ctyFromZod(z.boolean()), "bool");
  assert.equal(ctyFromZod(z.enum(["a", "b"])), "string");
});

test("z.set(scalar) → cty set; z.array → list; z.record → map", () => {
  assert.deepEqual(ctyFromZod(z.set(z.string())), ["set", "string"]);
  assert.deepEqual(ctyFromZod(z.array(z.string())), ["list", "string"]);
  assert.deepEqual(ctyFromZod(z.record(z.string(), z.number())), ["map", "number"]);
});

test("z.set of objects is rejected (JS Set can't dedup by value)", () => {
  assert.throws(() => ctyFromZod(z.set(z.object({ a: z.string() }))), /z\.set/);
});

test("a z.set field lands as a cty set attribute", () => {
  const schema = z.object({ tags: z.set(z.string()) });
  assert.deepEqual(byName(attrs(schema), "tags").type, ["set", "string"]);
});

test("the `set` disposition turns an array attribute into a cty set", () => {
  const schema = z.object({ tags: z.array(z.string()) });
  // via .meta({ set })
  assert.deepEqual(byName(attrs(z.object({ tags: z.array(z.string()).meta({ set: true }) })), "tags").type, [
    "set",
    "string",
  ]);
  // via the legacy array form
  assert.deepEqual(byName(attrs(schema, { set: ["tags"] }), "tags").type, ["set", "string"]);
});

test("dispositions read from per-field .meta() (algebraic types define everything)", () => {
  const schema = z.object({
    name: z.string().meta({ forceNew: true }),
    arn: z.string().meta({ computed: true }),
    secret: z.string().meta({ sensitive: true, writeOnly: true }),
    old: z.string().meta({ deprecated: true }),
  });
  const a = attrs(schema);
  assert.equal(byName(a, "name").forceNew, true);
  assert.equal(byName(a, "arn").computed, true);
  assert.equal(byName(a, "arn").required, false, "computed forces not-required");
  assert.equal(byName(a, "secret").sensitive, true);
  assert.equal(byName(a, "secret").writeOnly, true);
  assert.equal(byName(a, "old").deprecated, true);
});

test("the `identity` disposition reads from .meta() and the legacy array", () => {
  const schema = z.object({
    id: z.string().meta({ computed: true, identity: true }),
    arn: z.string().meta({ computed: true }),
  });
  const a = attrs(schema);
  assert.equal(byName(a, "id").identity, true, "meta({ identity }) marks the attribute");
  assert.equal(byName(a, "arn").identity, false, "unmarked attributes are not identity");
  // The legacy array form is honored and merged in too.
  const b = attrs(z.object({ name: z.string() }), { identity: ["name"] });
  assert.equal(byName(b, "name").identity, true, "identity array marks the attribute");
});

test(".meta() survives wrappers like .optional()", () => {
  const schema = z.object({ note: z.string().meta({ computed: true }).optional() });
  assert.equal(byName(attrs(schema), "note").computed, true);
});

test("meta and the legacy arrays merge (both honored)", () => {
  const schema = z.object({ a: z.string().meta({ computed: true }), b: z.string() });
  const a = attrs(schema, { forceNew: ["b"] });
  assert.equal(byName(a, "a").computed, true);
  assert.equal(byName(a, "b").forceNew, true);
});

test("blocks via .meta({ block }); set blocks via .meta({ block, set })", () => {
  const Rule = z.object({ port: z.number() });
  const schema = z.object({
    listed: z.array(Rule).meta({ block: true }),
    grouped: z.array(Rule).meta({ block: true, set: true }),
    one: Rule.meta({ block: true }),
  });
  const b = blocks(schema);
  assert.equal(byName(b, "listed").nesting, "list");
  assert.equal(byName(b, "grouped").nesting, "set", "a set-marked array block is an unordered set block");
  assert.equal(byName(b, "one").nesting, "single");
  assert.equal(byName(b, "one").minItems, 1, "a bare-object block is required");
  assert.equal(byName(b, "grouped").block.attributes[0].name, "port");
});

test("reviveSets turns the JSON arrays backing z.set fields into JS Sets", () => {
  const schema = z.object({
    tags: z.set(z.string()),
    nested: z.object({ ports: z.set(z.number()) }),
    list: z.array(z.string()),
  });
  const revived = reviveSets(schema, {
    tags: ["a", "b"],
    nested: { ports: [1, 2] },
    list: ["x", "y"],
  });
  assert.ok(revived.tags instanceof Set, "tags revived to a Set");
  assert.deepEqual([...revived.tags].sort(), ["a", "b"]);
  assert.ok(revived.nested.ports instanceof Set, "nested set revived");
  assert.ok(Array.isArray(revived.list), "a plain array stays an array");
});

test("toWireJson renders JS Sets back to arrays for the wire", () => {
  const out = JSON.parse(toWireJson({ tags: new Set(["a", "b"]), n: 1 }));
  assert.deepEqual(out.tags, ["a", "b"]);
  assert.equal(out.n, 1);
});

test("searchKeysOf reads `.meta({ searchKey })` fields", () => {
  const schema = z.object({
    cluster: z.string().meta({ searchKey: true }),
    region: z.string().meta({ searchKey: true }),
    id: z.string(),
  });
  assert.deepEqual(searchKeysOf(schema).sort(), ["cluster", "region"]);
});

test("functionSignatureJson derives ordered params + return from a Zod params object", () => {
  const sig = JSON.parse(
    functionSignatureJson(
      z.object({ name: z.string(), count: z.number().optional() }),
      z.string(),
      { summary: "build it" },
    ),
  );
  assert.deepEqual(paramNames(z.object({ name: z.string(), count: z.number() })), ["name", "count"]);
  assert.deepEqual(sig.params, [
    { name: "name", type: "string", allowNull: false },
    { name: "count", type: "number", allowNull: true },
  ]);
  assert.equal(sig.return, "string");
  assert.equal(sig.summary, "build it");
  assert.equal(sig.variadic, undefined, "no variadic unless given");
});

test("functionSignatureJson emits a trailing variadic param when given", () => {
  const sig = JSON.parse(
    functionSignatureJson(z.object({ separator: z.string() }), z.string(), {
      variadic: z.string(),
    }),
  );
  assert.deepEqual(sig.params, [{ name: "separator", type: "string", allowNull: false }]);
  assert.deepEqual(sig.variadic, { name: "varargs", type: "string", allowNull: false });
});

test("a transform (quotient type) derives its cty from the input type", () => {
  // The transform canonicalizes (parse-don't-validate); its structural cty is the
  // input type, and the canonicalization drives diff suppression in the plan hook.
  assert.equal(ctyFromZod(z.string().transform((s) => s.toLowerCase())), "string");
  assert.equal(ctyFromZod(z.string().transform((s) => s.length)), "string");
});

test("transformFields finds quotient fields and their canonicalizer", () => {
  const schema = z.object({
    id: z.string().transform((s) => s.toLowerCase()),
    name: z.string(),
  });
  const fields = transformFields(schema);
  assert.deepEqual(
    fields.map((f) => f.name),
    ["id"],
  );
  // parse() runs the transform = canonicalize; differently-cased inputs match.
  assert.equal(fields[0].schema.parse("ARN"), fields[0].schema.parse("arn"));
});
