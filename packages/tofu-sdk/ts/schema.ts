/**
 * Schema derivation: Zod model → `cty` type constraint + dispositions (the shape
 * the native addon consumes). This is a **pure** module — it imports no native
 * binding — so it can be unit-tested on its own.
 *
 * ## Everything in the Zod type, as much as possible
 *
 * The cty type is read **directly from the Zod type** (via Zod 4's `_zod.def`),
 * not through `z.toJSONSchema` (which throws on `.transform` and loses structure).
 * The Terraform-specific dispositions Zod has no built-in for — `computed`,
 * `forceNew`, `sensitive`, `writeOnly`, `deprecated`, `block`, `set` — are read
 * from **per-field `.meta({...})`** (typed via the {@link TfMeta} augmentation of
 * Zod's `GlobalMeta`), so a field carries its own meaning:
 *
 * ```ts
 * z.object({
 *   name: z.string().meta({ forceNew: true }),
 *   arn:  z.string().meta({ computed: true }),
 *   tags: z.set(z.string()),            // unordered, type-native
 * })
 * ```
 *
 * The older out-of-band disposition **arrays** (`{ computed: ["arn"], … }`) still
 * work and are merged in, so existing providers keep compiling; new code should
 * prefer the field metadata.
 *
 * ## The algebraic design law over the JSON seam
 *
 * "Order must not matter" is modeled as a **set**, so Terraform's structural diff
 * is order-insensitive for free (no dedup code) — the Zod analog of `HashSet<T>`
 * over `Vec<T>`. A scalar `z.set(T)` derives to a cty `set` directly (the boundary
 * marshals JS `Set` ⇄ JSON array; see {@link reviveSets}). A `z.set` of objects is
 * rejected — a JS `Set` can't dedup objects by value — so for object/array sets
 * you tag a `z.array(...)` field `.meta({ set: true })` and the value stays a
 * plain array while the declared type becomes a cty `set`.
 */

import { z } from "zod";

/** Terraform dispositions attachable to a Zod field via `.meta({ … })`. */
export interface TfMeta {
  /** Provider-computed (read-only): filled by handlers, "known after apply". */
  computed?: boolean;
  /** A change forces the resource to be replaced. */
  forceNew?: boolean;
  /** The value should be redacted in UI/logs. */
  sensitive?: boolean;
  /** Supplied at apply time but never persisted to state (e.g. a secret). */
  writeOnly?: boolean;
  /** Marked deprecated (a boolean notice). */
  deprecated?: boolean;
  /**
   * Part of the resource's **identity** — the stable key Terraform tracks the
   * resource by (import-by-identity, cross-config tracking). Mirrors Rust's
   * `#[facet(terraform::identity)]`; usually a computed `id`/`arn` or a
   * `forceNew` natural key.
   */
  identity?: boolean;
  /** Render as a nested HCL block (`name { … }`) instead of an attribute. */
  block?: boolean;
  /**
   * Treat an **array** field as an unordered cty `set` (no spurious diff on
   * reorder). Redundant on a `z.set(scalar)` field, which is already a set.
   */
  set?: boolean;
  /**
   * Marks a field of a **plural data source** as a query input (the rest of the
   * model is the computed result element). The Zod-field equivalent of the
   * `searchKeys` array.
   */
  searchKey?: boolean;
}

// Make `.meta({ computed: true })` type-checked: Terraform keys join Zod's
// built-in `GlobalMeta` (which carries `title`/`description`/…).
declare module "zod" {
  interface GlobalMeta extends TfMeta {}
}

/** A `cty` type constraint in its JSON form, the shape the native addon takes. */
export type CtyType =
  | "string"
  | "number"
  | "bool"
  | "dynamic"
  | ["list", CtyType]
  | ["set", CtyType]
  | ["map", CtyType]
  | ["object", Record<string, CtyType>]
  | ["object", Record<string, CtyType>, string[]];

/** Field names of a Zod object schema, as a string-key union. */
export type FieldName<S extends z.ZodObject<z.ZodRawShape>> = keyof z.infer<S> & string;

/**
 * The legacy out-of-band dispositions, keyed to real schema fields. Prefer
 * per-field `.meta({ … })`; these arrays are merged in for back-compat.
 */
export interface Dispositions<S extends z.ZodObject<z.ZodRawShape>> {
  /** Provider-computed (read-only) attributes. */
  computed?: FieldName<S>[];
  /** Attributes whose change forces the resource to be replaced. */
  forceNew?: FieldName<S>[];
  /** Attributes whose values should be redacted. */
  sensitive?: FieldName<S>[];
  /** Write-only attributes: supplied at apply time, never persisted. */
  writeOnly?: FieldName<S>[];
  /** Attributes marked deprecated. */
  deprecated?: FieldName<S>[];
  /** Attributes forming the resource's identity (import-by-identity, tracking). */
  identity?: FieldName<S>[];
  /** Array fields to derive as unordered cty `set`s rather than lists. */
  set?: FieldName<S>[];
  /** Fields to render as nested blocks (`name { … }`). */
  blocks?: FieldName<S>[];
}

/** One cty attribute in the schema JSON the native addon consumes. */
export interface AttributeJson {
  name: string;
  type: CtyType;
  required: boolean;
  optional: boolean;
  computed: boolean;
  forceNew: boolean;
  sensitive: boolean;
  writeOnly: boolean;
  deprecated: boolean;
  identity: boolean;
}

/** One nested-block descriptor in the schema JSON. */
export interface BlockJson {
  name: string;
  nesting: "single" | "list" | "set" | "map";
  minItems: number;
  maxItems: number;
  block: { attributes: AttributeJson[]; blocks: BlockJson[] };
}

// --- Zod introspection -------------------------------------------------------

/** The slice of Zod 4's internal type def we read. */
interface ZodDef {
  type: string;
  innerType?: z.ZodType;
  element?: z.ZodType;
  valueType?: z.ZodType;
  shape?: Record<string, z.ZodType>;
  values?: unknown[];
  in?: z.ZodType;
}

/** Read a Zod type's internal def (Zod 4 exposes it at `_zod.def`). */
function zdef(t: z.ZodType): ZodDef {
  const anyT = t as unknown as { _zod?: { def: ZodDef }; def?: ZodDef; _def?: ZodDef };
  const d = anyT._zod?.def ?? anyT.def ?? anyT._def;
  if (!d) throw new Error("could not read Zod type definition (unexpected Zod version)");
  return d;
}

/** Read a field's `.meta()` (Zod 4), or `undefined`. */
function readMeta(t: z.ZodType): TfMeta | undefined {
  return (t as unknown as { meta?: () => TfMeta | undefined }).meta?.();
}

/**
 * The optionality wrappers Zod stacks around a field. `optional`/`default` make
 * a field optional in cty terms; `nullable` does not (cty carries null at the
 * value level); the rest are transparent.
 */
const WRAPPERS = new Set([
  "optional",
  "default",
  "prefault",
  "nullable",
  "readonly",
  "catch",
  "nonoptional",
]);

/**
 * Strip the optionality wrappers off a Zod type, returning the core type, whether
 * the field is **optional**, and the merged `.meta()` gathered across the chain
 * (a `.meta()` on the field survives wrappers like `.optional()`, which hide it
 * on an inner type).
 */
function peel(t: z.ZodType): { core: z.ZodType; optional: boolean; meta: TfMeta } {
  let optional = false;
  let cur = t;
  let meta: TfMeta = {};
  const seen = new Set<z.ZodType>();
  for (;;) {
    if (seen.has(cur)) break;
    seen.add(cur);
    const m = readMeta(cur);
    if (m) meta = { ...m, ...meta }; // outer (more specific) wins
    const d = zdef(cur);
    if (!WRAPPERS.has(d.type) || !d.innerType) return { core: cur, optional, meta };
    if (d.type === "optional" || d.type === "default" || d.type === "prefault") optional = true;
    if (d.type === "nonoptional") optional = false;
    cur = d.innerType;
  }
  return { core: cur, optional, meta };
}

/** Whether a Zod type is a cty scalar (string/number/bool family). */
function isScalar(t: z.ZodType): boolean {
  const d = zdef(peel(t).core);
  return ["string", "number", "int", "bigint", "boolean", "enum", "literal", "date"].includes(
    d.type,
  );
}

/** Map a Zod type to its `cty` type constraint. */
export function ctyFromZod(t: z.ZodType): CtyType {
  const { core } = peel(t);
  const d = zdef(core);
  switch (d.type) {
    case "string":
    case "enum":
    case "date": // cty has no date type; carry as a string
      return "string";
    case "literal":
      return literalCty(d.values ?? []);
    case "number":
    case "int":
    case "bigint":
      return "number";
    case "boolean":
      return "bool";
    case "any":
    case "unknown":
      return "dynamic";
    case "array":
      return ["list", ctyFromZod(d.element!)];
    case "set": {
      // A scalar JS `Set` round-trips as a JSON array (the boundary marshals it).
      // An object `Set` can't dedup by value — reject; use an array `.meta({set})`.
      const element = d.valueType!;
      if (!isScalar(element)) {
        throw new Error(
          "z.set(...) of objects/collections is not supported (a JS Set can't dedup " +
            "them by value); use z.array(...).meta({ set: true }) to derive a cty set",
        );
      }
      return ["set", ctyFromZod(element)];
    }
    case "record":
      return ["map", ctyFromZod(d.valueType!)];
    case "object": {
      const shape = d.shape ?? {};
      const fields: Record<string, CtyType> = {};
      const optional: string[] = [];
      for (const [name, ft] of Object.entries(shape)) {
        fields[name] = ctyFromZod(ft);
        if (peel(ft).optional) optional.push(name);
      }
      return optional.length > 0 ? ["object", fields, optional] : ["object", fields];
    }
    case "pipe":
      // A transform / codec is a **quotient type** (parse-don't-validate): its
      // structural cty is its *input* type, and the canonicalization it performs
      // drives diff suppression in the plan hook (`keepPrior`), not the schema.
      return ctyFromZod(d.in ?? z.unknown());
    case "transform":
    case "codec":
      throw new Error(
        `a bare ${d.type} has no input type to derive a cty from; ` +
          "use it as the tail of a pipe (e.g. z.string().transform(...))",
      );
    default:
      throw new Error(`unsupported Zod type for cty: ${d.type}`);
  }
}

/** Derive a cty scalar from a `z.literal(...)`'s value(s). */
function literalCty(values: unknown[]): CtyType {
  const v = values[0];
  switch (typeof v) {
    case "number":
      return "number";
    case "boolean":
      return "bool";
    default:
      return "string";
  }
}

/** Rewrite a `list` cty type to a `set` (for a set-marked array field). */
function asSet(type: CtyType): CtyType {
  return Array.isArray(type) && type[0] === "list" ? ["set", type[1]] : type;
}

/** The cty type of object field `key` (used by the plural data source wrapper). */
export function fieldCty(schema: z.ZodObject<z.ZodRawShape>, key: string): CtyType {
  const shape = zdef(schema).shape ?? {};
  return shape[key] ? ctyFromZod(shape[key]) : "string";
}

/** Field names tagged `.meta({ searchKey: true })` (plural data-source inputs). */
export function searchKeysOf(schema: z.ZodObject<z.ZodRawShape>): string[] {
  const shape = zdef(schema).shape ?? {};
  return Object.entries(shape)
    .filter(([, ft]) => peel(ft).meta.searchKey === true)
    .map(([name]) => name);
}

/** Ordered field names of a params object (positional parameter order). */
export function paramNames(params: z.ZodObject<z.ZodRawShape>): string[] {
  return Object.keys(zdef(params).shape ?? {});
}

/**
 * Top-level fields that are **quotient types** — a transform / codec (a `pipe`)
 * whose constructor canonicalizes (parse-don't-validate). Returns each field's
 * name and full schema; `schema.parse(rawValue)` yields the canonical form, used
 * to suppress spurious diffs ("keep prior" when canonical forms match).
 */
export function transformFields(
  schema: z.ZodObject<z.ZodRawShape>,
): { name: string; schema: z.ZodType }[] {
  const shape = zdef(schema).shape ?? {};
  return Object.entries(shape)
    .filter(([, ft]) => ["pipe", "transform", "codec"].includes(zdef(peel(ft).core).type))
    .map(([name, ft]) => ({ name, schema: ft }));
}

/** An attribute path: a sequence of attribute names and list/set indices. */
export type Path = (string | number)[];

/** What a `modifyPlan` hook returns: attribute paths to force-replace, mark
 * unknown, or reset to the prior value (diff suppression). */
export interface PlanModificationResult {
  /** Paths whose change should force the resource to be replaced. */
  replace?: Path[];
  /** Paths to mark unknown (computed-by-rule, known after apply). */
  unknown?: Path[];
  /** Paths to reset to the prior value (suppress a spurious diff). */
  keepPrior?: Path[];
}

/** Whether a Zod type admits null (`.optional()` / `.nullable()` / `.default()`). */
function allowsNull(t: z.ZodType): boolean {
  let cur = t;
  const seen = new Set<z.ZodType>();
  for (;;) {
    if (seen.has(cur)) return false;
    seen.add(cur);
    const d = zdef(cur);
    if (["nullable", "optional", "default", "prefault"].includes(d.type)) return true;
    if (WRAPPERS.has(d.type) && d.innerType) cur = d.innerType;
    else return false;
  }
}

/**
 * Build the function-signature JSON the addon consumes, from a params **object**
 * (its key order is the positional parameter order), a return schema, and an
 * optional trailing **variadic** element type (a single uniform schema, mirroring
 * Rust's `VariadicFunction::VarArg`).
 */
export function functionSignatureJson(
  params: z.ZodObject<z.ZodRawShape>,
  returns: z.ZodType,
  opts: { summary?: string; description?: string; variadic?: z.ZodType } = {},
): string {
  const shape = zdef(params).shape ?? {};
  const paramList = Object.entries(shape).map(([name, ft]) => ({
    name,
    type: ctyFromZod(ft),
    allowNull: allowsNull(ft),
  }));
  return JSON.stringify({
    params: paramList,
    variadic: opts.variadic
      ? { name: "varargs", type: ctyFromZod(opts.variadic), allowNull: allowsNull(opts.variadic) }
      : undefined,
    return: ctyFromZod(returns),
    summary: opts.summary ?? "",
    description: opts.description ?? "",
  });
}

// --- schema JSON -------------------------------------------------------------

/** Derive plain cty attributes from an object Zod type (a block's contents). */
function attributesFromObject(obj: z.ZodType): AttributeJson[] {
  const shape = zdef(peel(obj).core).shape ?? {};
  return Object.entries(shape).map(([name, ft]) => {
    const { optional, meta } = peel(ft);
    const computed = meta.computed === true;
    return {
      name,
      type: meta.set ? asSet(ctyFromZod(ft)) : ctyFromZod(ft),
      required: !optional && !computed,
      optional: optional && !computed,
      computed,
      forceNew: meta.forceNew === true,
      sensitive: meta.sensitive === true,
      writeOnly: meta.writeOnly === true,
      deprecated: meta.deprecated === true,
      identity: meta.identity === true,
    };
  });
}

/**
 * Build a nested-block descriptor for `name` from its Zod type. An `array` field
 * is a repeatable block — `set`-nested when `markedSet`, else `list`; a bare
 * object is a `single` block (required when the field itself is required).
 */
function blockFromField(name: string, ft: z.ZodType, markedSet: boolean): BlockJson {
  const { core, optional } = peel(ft);
  const d = zdef(core);
  if (d.type === "array") {
    return {
      name,
      nesting: markedSet ? "set" : "list",
      minItems: 0,
      maxItems: 0,
      block: { attributes: attributesFromObject(d.element!), blocks: [] },
    };
  }
  return {
    name,
    nesting: "single",
    minItems: optional ? 0 : 1,
    maxItems: 1,
    block: { attributes: attributesFromObject(core), blocks: [] },
  };
}

/**
 * Build the `{ attributes, blocks }` schema JSON the native addon consumes,
 * reading dispositions from per-field `.meta()` merged with the legacy arrays.
 */
export function schemaJson(
  schema: z.ZodObject<z.ZodRawShape>,
  dispositions: Dispositions<z.ZodObject<z.ZodRawShape>> = {},
): string {
  const arr = (names?: string[]) => new Set<string>(names ?? []);
  const aComputed = arr(dispositions.computed);
  const aForceNew = arr(dispositions.forceNew);
  const aSensitive = arr(dispositions.sensitive);
  const aWriteOnly = arr(dispositions.writeOnly);
  const aDeprecated = arr(dispositions.deprecated);
  const aIdentity = arr(dispositions.identity);
  const aSet = arr(dispositions.set);
  const aBlocks = arr(dispositions.blocks);

  const shape = zdef(schema).shape ?? {};
  const attributes: AttributeJson[] = [];
  const blocks: BlockJson[] = [];
  for (const [name, ft] of Object.entries(shape)) {
    const { optional, meta } = peel(ft);
    const isBlock = meta.block === true || aBlocks.has(name);
    const isSet = meta.set === true || aSet.has(name);
    if (isBlock) {
      blocks.push(blockFromField(name, ft, isSet));
      continue;
    }
    const isComputed = meta.computed === true || aComputed.has(name);
    const isRequired = !optional && !isComputed;
    const type = isSet ? asSet(ctyFromZod(ft)) : ctyFromZod(ft);
    attributes.push({
      name,
      type,
      required: isRequired,
      optional: !isRequired && !isComputed,
      computed: isComputed,
      forceNew: meta.forceNew === true || aForceNew.has(name),
      sensitive: meta.sensitive === true || aSensitive.has(name),
      writeOnly: meta.writeOnly === true || aWriteOnly.has(name),
      deprecated: meta.deprecated === true || aDeprecated.has(name),
      identity: meta.identity === true || aIdentity.has(name),
    });
  }
  return JSON.stringify({ attributes, blocks });
}

// --- boundary marshaling (JS Set ⇄ JSON array) -------------------------------

/**
 * Revive a parsed JSON value into the shape `z.infer` promises, converting the
 * JSON **arrays** that back `z.set(scalar)` fields into JS `Set`s (recursively,
 * through objects/arrays/records). The inverse — `Set` → array on the way out —
 * is a plain `JSON.stringify` replacer (see {@link setReplacer}); only the input
 * direction needs the schema to know *which* arrays are sets.
 */
export function reviveSets<T>(schema: z.ZodType, value: unknown): T {
  return revive(schema, value) as T;
}

function revive(t: z.ZodType, value: unknown): unknown {
  if (value === null || value === undefined) return value;
  const { core } = peel(t);
  const d = zdef(core);
  switch (d.type) {
    case "set":
      return Array.isArray(value)
        ? new Set((value as unknown[]).map((v) => revive(d.valueType!, v)))
        : value;
    case "array":
      return Array.isArray(value) ? (value as unknown[]).map((v) => revive(d.element!, v)) : value;
    case "object": {
      if (typeof value !== "object") return value;
      const shape = d.shape ?? {};
      const out: Record<string, unknown> = { ...(value as Record<string, unknown>) };
      for (const [k, ft] of Object.entries(shape)) {
        if (k in out) out[k] = revive(ft, out[k]);
      }
      return out;
    }
    case "record": {
      if (typeof value !== "object") return value;
      const out: Record<string, unknown> = {};
      for (const [k, v] of Object.entries(value as Record<string, unknown>)) {
        out[k] = revive(d.valueType!, v);
      }
      return out;
    }
    default:
      return value;
  }
}

/** A `JSON.stringify` replacer that renders any JS `Set` as an array (the wire
 * form of a cty set). Used for every handler result so `z.set` fields serialize. */
export function setReplacer(_key: string, value: unknown): unknown {
  return value instanceof Set ? Array.from(value) : value;
}

/** `JSON.stringify` with set-aware encoding (a `Set` → its array form). */
export function toWireJson(value: unknown): string {
  return JSON.stringify(value ?? null, setReplacer);
}
