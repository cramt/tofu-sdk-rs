/**
 * `@tofu-sdk/core` — write Terraform/OpenTofu providers in TypeScript.
 *
 * Schemas are [Zod](https://zod.dev) objects: you get runtime validation and
 * inferred handler types for free, and the cty schema Terraform needs is derived
 * from them. The Terraform-only dispositions that Zod can't express — `computed`,
 * `forceNew`, `sensitive` — are given as arrays of field names, and those arrays
 * are **type-checked against the schema** (a typo is a compile error).
 *
 * ```ts
 * import { z } from "zod";
 * import { Provider } from "@tofu-sdk/core";
 *
 * const Bucket = z.object({ name: z.string(), arn: z.string() });
 *
 * await new Provider()
 *   .resource("aws_s3_bucket", {
 *     schema: Bucket,
 *     forceNew: ["name"],     // only "name" | "arn" type-checks here
 *     computed: ["arn"],
 *     async create(planned) { // planned: { name: string; arn: string }
 *       return { ...planned, arn: `arn:aws:s3:::${planned.name}` };
 *     },
 *   })
 *   .serve();
 * ```
 */

import { z } from "zod";

// The native addon's generated loader, loaded at runtime and hand-typed (its
// auto-generated `.d.ts` is intentionally not imported).
type RawHandler = (err: Error | null, input: string) => Promise<string>;

interface RawProvider {
  config(schemaJson: string, configure: RawHandler): void;
  resource(
    typeName: string,
    version: number,
    schemaJson: string,
    create: RawHandler,
    read: RawHandler,
    update: RawHandler,
    del: RawHandler,
    imp: RawHandler,
    upgrade: RawHandler,
    validate: RawHandler,
  ): void;
  dataSource(typeName: string, schemaJson: string, read: RawHandler, validate: RawHandler): void;
  ephemeral(
    typeName: string,
    schemaJson: string,
    open: RawHandler,
    renew: RawHandler,
    close: RawHandler,
    validate: RawHandler,
  ): void;
  serve(): Promise<void>;
}

interface RawBinding {
  Provider: new () => RawProvider;
}

// The native addon's generated loader (CommonJS). A *static* `require` keeps the
// binding analyzable by bundlers: rolldown/esbuild inline `binding/index.js` and
// only the platform `*.node` stays external (mark `/\.node$/` external and copy
// it next to the bundle). Un-bundled, this resolves to `<pkg>/binding/index.js`
// relative to this compiled file, exactly as before.
const native = require("../binding/index.js") as RawBinding;

// --- schema derivation (Zod -> cty) -----------------------------------------

/** A `cty` type constraint in its JSON form, the shape the native addon takes. */
type CtyType =
  | "string"
  | "number"
  | "bool"
  | "dynamic"
  | ["list", CtyType]
  | ["set", CtyType]
  | ["map", CtyType]
  | ["object", Record<string, CtyType>]
  | ["object", Record<string, CtyType>, string[]];

type JsonSchema = {
  type?: string | string[];
  properties?: Record<string, JsonSchema>;
  required?: string[];
  items?: JsonSchema;
  additionalProperties?: boolean | JsonSchema;
  anyOf?: JsonSchema[];
};

/** Map a (Zod-produced) JSON Schema node to a `cty` type. */
function jsonSchemaToCty(node: JsonSchema): CtyType {
  // Nullable unwraps to the inner type (cty carries null at the value level).
  if (node.anyOf) {
    const inner = node.anyOf.find((s) => s.type !== "null") ?? node.anyOf[0];
    return jsonSchemaToCty(inner);
  }
  const type = Array.isArray(node.type)
    ? node.type.find((t) => t !== "null")
    : node.type;

  switch (type) {
    case "string":
      return "string";
    case "number":
    case "integer":
      return "number";
    case "boolean":
      return "bool";
    case "array":
      return ["list", jsonSchemaToCty(node.items ?? {})];
    case "object": {
      const props = node.properties ?? {};
      const names = Object.keys(props);
      if (names.length > 0) {
        const fields: Record<string, CtyType> = {};
        for (const name of names) fields[name] = jsonSchemaToCty(props[name]);
        const required = new Set(node.required ?? []);
        const optional = names.filter((n) => !required.has(n));
        return optional.length > 0 ? ["object", fields, optional] : ["object", fields];
      }
      if (node.additionalProperties && typeof node.additionalProperties === "object") {
        return ["map", jsonSchemaToCty(node.additionalProperties)];
      }
      return ["object", {}];
    }
    default:
      throw new Error(`unsupported schema type for cty: ${JSON.stringify(node.type)}`);
  }
}

/** Field names of a Zod object schema, as a string-key union. */
type FieldName<S extends z.ZodObject<z.ZodRawShape>> = keyof z.infer<S> & string;

/** The Terraform dispositions Zod can't express, keyed to real schema fields. */
interface Dispositions<S extends z.ZodObject<z.ZodRawShape>> {
  /** Provider-computed (read-only) attributes. */
  computed?: FieldName<S>[];
  /** Attributes whose change forces the resource to be replaced. */
  forceNew?: FieldName<S>[];
  /** Attributes whose values should be redacted. */
  sensitive?: FieldName<S>[];
  /**
   * Write-only attributes: supplied at apply time but never persisted to state
   * (e.g. secrets). The provider runtime nulls them out of every returned state,
   * so a handler reads the real value only from the apply-time config. Cannot be
   * combined with `computed`.
   */
  writeOnly?: FieldName<S>[];
  /**
   * Fields to render as nested **blocks** (`name { … }`) instead of object/list
   * attributes (`name = …`). Each named field must be an object (a single block)
   * or an array of objects (a repeatable block); on the wire a block is just an
   * object/list, so handlers see the field unchanged. Inner attributes are
   * derived from the element's shape (required follows the Zod schema; per-field
   * `computed`/`sensitive` inside a block are not expressible here).
   */
  blocks?: FieldName<S>[];
}

/** A validation diagnostic returned from a `validate` hook. */
export interface Diagnostic {
  /** Severity; defaults to `"error"`. */
  severity?: "error" | "warning";
  /** Short, one-line summary. */
  summary: string;
  /** Optional longer explanation. */
  detail?: string;
  /** Optional attribute path (a sequence of names) to point the diagnostic at. */
  attribute?: string[];
}

/**
 * A `validate` hook: inspect a config and return diagnostics (or nothing).
 * Runs before planning. Attributes the user did not set — or whose values are
 * not yet known (references to other resources) — arrive as `null`/`undefined`,
 * so guard before validating them.
 */
type Validate<C> = (config: C) => Diagnostic[] | void | Promise<Diagnostic[] | void>;

/** One cty attribute in the schema JSON the native addon consumes. */
interface AttributeJson {
  name: string;
  type: CtyType;
  required: boolean;
  optional: boolean;
  computed: boolean;
  forceNew: boolean;
  sensitive: boolean;
  writeOnly: boolean;
}

/** One nested-block descriptor in the schema JSON. */
interface BlockJson {
  name: string;
  nesting: "single" | "list" | "set" | "map";
  minItems: number;
  maxItems: number;
  block: { attributes: AttributeJson[]; blocks: BlockJson[] };
}

/** Unwrap a nullable JSON Schema node (`anyOf` / `["T","null"]`) to its core. */
function unwrapNullable(node: JsonSchema): JsonSchema {
  if (node.anyOf) return node.anyOf.find((s) => s.type !== "null") ?? node.anyOf[0];
  return node;
}

/** Derive plain cty attributes from an object node's properties (block contents). */
function attributesFromObject(node: JsonSchema): AttributeJson[] {
  const props = node.properties ?? {};
  const required = new Set(node.required ?? []);
  return Object.entries(props).map(([name, prop]) => ({
    name,
    type: jsonSchemaToCty(prop),
    required: required.has(name),
    optional: !required.has(name),
    computed: false,
    forceNew: false,
    sensitive: false,
    writeOnly: false,
  }));
}

/**
 * Build a nested-block descriptor for `name` from its Zod-derived schema node.
 * An array field is a repeatable `list` block; a bare object is a `single` block
 * (required when the field itself is required).
 */
function blockFromField(name: string, prop: JsonSchema, fieldRequired: boolean): BlockJson {
  const node = unwrapNullable(prop);
  const type = Array.isArray(node.type) ? node.type.find((t) => t !== "null") : node.type;
  if (type === "array") {
    const element = unwrapNullable(node.items ?? {});
    return {
      name,
      nesting: "list",
      minItems: 0,
      maxItems: 0,
      block: { attributes: attributesFromObject(element), blocks: [] },
    };
  }
  return {
    name,
    nesting: "single",
    minItems: fieldRequired ? 1 : 0,
    maxItems: 1,
    block: { attributes: attributesFromObject(node), blocks: [] },
  };
}

/** Build the `{ attributes, blocks }` schema JSON the native addon consumes. */
function schemaJson(
  schema: z.ZodObject<z.ZodRawShape>,
  dispositions: Dispositions<z.ZodObject<z.ZodRawShape>> = {},
): string {
  const json = z.toJSONSchema(schema) as JsonSchema;
  const required = new Set(json.required ?? []);
  const computed = new Set<string>(dispositions.computed ?? []);
  const forceNew = new Set<string>(dispositions.forceNew ?? []);
  const sensitive = new Set<string>(dispositions.sensitive ?? []);
  const writeOnly = new Set<string>(dispositions.writeOnly ?? []);
  const blockNames = new Set<string>(dispositions.blocks ?? []);

  const attributes: AttributeJson[] = [];
  const blocks: BlockJson[] = [];
  for (const [name, prop] of Object.entries(json.properties ?? {})) {
    if (blockNames.has(name)) {
      blocks.push(blockFromField(name, prop, required.has(name)));
      continue;
    }
    const isComputed = computed.has(name);
    const isRequired = required.has(name) && !isComputed;
    attributes.push({
      name,
      type: jsonSchemaToCty(prop),
      required: isRequired,
      optional: !isRequired && !isComputed,
      computed: isComputed,
      forceNew: forceNew.has(name),
      sensitive: sensitive.has(name),
      writeOnly: writeOnly.has(name),
    });
  }
  return JSON.stringify({ attributes, blocks });
}

/** Validate a handler's return value against its schema, surfacing failures. */
function validateOut<S extends z.ZodType>(schema: S, value: unknown, ctx: string): z.infer<S> {
  const result = schema.safeParse(value);
  if (!result.success) {
    throw new Error(`${ctx} produced an invalid value: ${result.error.message}`);
  }
  return result.data;
}

// --- author-facing definitions ---------------------------------------------

/** A managed resource. `S` is the Zod model; `create` is required. */
export interface Resource<S extends z.ZodObject<z.ZodRawShape>> extends Dispositions<S> {
  schema: S;
  /**
   * The current state-schema version (default 0). Raise it when a schema change
   * needs migrating stored state, and implement `upgrade`.
   */
  version?: number;
  /** Create the resource and return its new state (computed fields filled). */
  create(planned: z.infer<S>): Promise<z.infer<S>>;
  /** Refresh state; return `null` if the resource no longer exists. */
  read?(current: z.infer<S>): Promise<z.infer<S> | null>;
  /** Update in place and return the new state. */
  update?(planned: z.infer<S>, prior: z.infer<S>): Promise<z.infer<S>>;
  /** Delete the resource. */
  delete?(prior: z.infer<S>): Promise<void>;
  /** Import an existing resource by ID, returning its state (then refreshed via `read`). */
  import?(id: string): Promise<z.infer<S>>;
  /**
   * Migrate stored state written at `fromVersion` to the current schema.
   * `prior` is the raw stored state (untyped — it predates the current schema).
   */
  upgrade?(fromVersion: number, prior: unknown): Promise<z.infer<S>>;
  /** Validate the configuration, returning diagnostics (or nothing). */
  validate?: Validate<z.infer<S>>;
}

/** A singular read-only data source: given a config, produce a state. */
export interface DataSource<S extends z.ZodObject<z.ZodRawShape>> extends Dispositions<S> {
  schema: S;
  read(config: z.infer<S>): Promise<z.infer<S>>;
  /** Validate the configuration, returning diagnostics (or nothing). */
  validate?: Validate<z.infer<S>>;
}

/**
 * A plural read-only data source: a lookup by `searchKeys` resolving to a
 * `results` list. `schema` describes one element; `searchKeys` (type-checked
 * against the schema) names the element fields that are query inputs.
 */
export interface DataSourceList<S extends z.ZodObject<z.ZodRawShape>> {
  schema: S;
  searchKeys: FieldName<S>[];
  list(query: z.infer<S>): Promise<z.infer<S>[]>;
}

/** What an ephemeral `open` produces: the result plus an optional handle/deadline. */
export interface EphemeralOpen<S extends z.ZodObject<z.ZodRawShape>> {
  /** The result value (computed fields filled), validated against the schema. */
  result: z.infer<S>;
  /**
   * An opaque handle to hand to `renew`/`close` — they receive *only* this
   * string (not the config or result). Stash a lease ID, a created object's ID,
   * etc. Omit it for a pure reader that holds nothing.
   */
  private?: string;
  /**
   * When to renew, in milliseconds since the Unix epoch (e.g. `Date.now() +
   * 300_000`). Terraform calls `renew` before then. Omit for "never expires".
   */
  renewAt?: number;
}

/** What an ephemeral `renew` may refresh: a new handle and/or a new deadline. */
export interface EphemeralRenew {
  /** Replace the stored handle (omit to keep the existing one). */
  private?: string;
  /** Push the renewal deadline forward (ms since epoch). */
  renewAt?: number;
}

/**
 * An **ephemeral resource**: a value produced for the duration of a single
 * operation and never written to state. `open` runs during *both* plan and
 * apply, so keep it plan-safe. Because `renew`/`close` receive only the private
 * handle, stash whatever they need in `open`'s returned `private`.
 */
export interface Ephemeral<S extends z.ZodObject<z.ZodRawShape>> extends Dispositions<S> {
  schema: S;
  /** Open the resource: produce the result (and optionally a handle + renewal). */
  open(config: z.infer<S>): Promise<EphemeralOpen<S>>;
  /** Renew a lease before its `renewAt`. Receives the stashed handle. */
  renew?(handle: string): Promise<EphemeralRenew | void>;
  /** Tear the resource down. Receives the stashed handle. */
  close?(handle: string): Promise<void>;
  /** Validate the configuration, returning diagnostics (or nothing). */
  validate?: Validate<z.infer<S>>;
}

/** Provider-level configuration; `configure` runs once at `ConfigureProvider`. */
export interface ProviderConfig<S extends z.ZodObject<z.ZodRawShape>> {
  schema: S;
  configure(config: z.infer<S>): Promise<void>;
}

/** Adapt an async `A -> R` handler to the raw `(err, json) -> Promise<json>` form. */
function adapt<A, R>(fn: (arg: A) => Promise<R>): RawHandler {
  return async (err, input) => {
    if (err) throw err;
    const result = await fn(JSON.parse(input) as A);
    return JSON.stringify(result ?? null);
  };
}

/** Adapt a `validate` hook to the raw form, returning a JSON diagnostics array. */
function validateAdapter<C>(validate: Validate<C> | undefined): RawHandler {
  return async (err, input) => {
    if (err) throw err;
    if (!validate) return "[]";
    const diagnostics = (await validate(JSON.parse(input) as C)) ?? [];
    return JSON.stringify(diagnostics);
  };
}

/** A Terraform/OpenTofu provider authored in TypeScript. */
export class Provider {
  private readonly raw: RawProvider = new native.Provider();

  /** Declare the provider's configuration block and its `configure` handler. */
  config<S extends z.ZodObject<z.ZodRawShape>>(def: ProviderConfig<S>): this {
    this.raw.config(
      schemaJson(def.schema),
      adapt(async (cfg: z.infer<S>) => {
        await def.configure(cfg);
        return null;
      }),
    );
    return this;
  }

  /** Register a managed resource under `typeName`. */
  resource<S extends z.ZodObject<z.ZodRawShape>>(typeName: string, def: Resource<S>): this {
    type M = z.infer<S>;
    const create = adapt((planned: M) =>
      def.create(planned).then((s) => validateOut(def.schema, s, `resource ${typeName} create`)),
    );
    const read = adapt(async (current: M) => {
      if (!def.read) return current;
      const refreshed = await def.read(current);
      return refreshed === null
        ? null
        : validateOut(def.schema, refreshed, `resource ${typeName} read`);
    });
    const update: RawHandler = async (err, input) => {
      if (err) throw err;
      const { planned, prior } = JSON.parse(input) as { planned: M; prior: M };
      if (!def.update) {
        throw new Error(`resource "${typeName}" does not support in-place update`);
      }
      const next = await def.update(planned, prior);
      return JSON.stringify(validateOut(def.schema, next, `resource ${typeName} update`));
    };
    const del = adapt(async (prior: M) => {
      if (def.delete) await def.delete(prior);
      return null;
    });
    // The import handler's input is the raw ID string, not marshalled JSON.
    const imp: RawHandler = async (err, id) => {
      if (err) throw err;
      if (!def.import) {
        throw new Error(`resource "${typeName}" does not support import`);
      }
      const imported = await def.import(id);
      return JSON.stringify(validateOut(def.schema, imported, `resource ${typeName} import`));
    };
    const upgrade: RawHandler = async (err, input) => {
      if (err) throw err;
      const { fromVersion, priorState } = JSON.parse(input) as {
        fromVersion: number;
        priorState: unknown;
      };
      if (!def.upgrade) {
        throw new Error(
          `resource "${typeName}" has no state upgrade (stored version ${fromVersion})`,
        );
      }
      const next = await def.upgrade(fromVersion, priorState);
      return JSON.stringify(validateOut(def.schema, next, `resource ${typeName} upgrade`));
    };
    this.raw.resource(
      typeName,
      def.version ?? 0,
      schemaJson(def.schema, def),
      create,
      read,
      update,
      del,
      imp,
      upgrade,
      validateAdapter(def.validate),
    );
    return this;
  }

  /** Register a singular read-only data source under `typeName`. */
  dataSource<S extends z.ZodObject<z.ZodRawShape>>(typeName: string, def: DataSource<S>): this {
    this.raw.dataSource(
      typeName,
      schemaJson(def.schema, def),
      adapt((config: z.infer<S>) =>
        def.read(config).then((s) => validateOut(def.schema, s, `data source ${typeName} read`)),
      ),
      validateAdapter(def.validate),
    );
    return this;
  }

  /**
   * Register a plural read-only data source under `typeName`: the `searchKeys`
   * are query inputs and the result is a computed `results` list of objects
   * matching `schema`.
   */
  dataSourceList<S extends z.ZodObject<z.ZodRawShape>>(
    typeName: string,
    def: DataSourceList<S>,
  ): this {
    type M = z.infer<S>;
    const element = z.toJSONSchema(def.schema) as JsonSchema;
    const elementType = jsonSchemaToCty(element);
    // The wrapper block: the search keys as optional inputs, plus `results`.
    const attributes = def.searchKeys.map((key) => ({
      name: key,
      type: (element.properties ?? {})[key]
        ? jsonSchemaToCty((element.properties ?? {})[key])
        : ("string" as CtyType),
      required: false,
      optional: true,
      computed: false,
      forceNew: false,
      sensitive: false,
      writeOnly: false,
    }));
    attributes.push({
      name: "results",
      type: ["list", elementType] as CtyType,
      required: false,
      optional: false,
      computed: true,
      forceNew: false,
      sensitive: false,
      writeOnly: false,
    });

    const read: RawHandler = async (err, input) => {
      if (err) throw err;
      const config = JSON.parse(input) as M;
      const items = await def.list(config);
      const validated = items.map((item, i) =>
        validateOut(def.schema, item, `data source ${typeName} list[${i}]`),
      );
      const inputs: Record<string, unknown> = {};
      for (const key of def.searchKeys) inputs[key] = (config as Record<string, unknown>)[key];
      return JSON.stringify({ ...inputs, results: validated });
    };
    this.raw.dataSource(typeName, JSON.stringify({ attributes }), read, validateAdapter(undefined));
    return this;
  }

  /**
   * Register an ephemeral resource under `typeName`: a value produced for the
   * duration of a single operation and never written to state. `open` runs during
   * plan *and* apply; `renew`/`close` receive only the handle `open` returned in
   * `private`.
   */
  ephemeral<S extends z.ZodObject<z.ZodRawShape>>(typeName: string, def: Ephemeral<S>): this {
    type M = z.infer<S>;
    const open = adapt(async (config: M) => {
      const opened = await def.open(config);
      return {
        result: validateOut(def.schema, opened.result, `ephemeral ${typeName} open`),
        private: opened.private,
        renewAt: opened.renewAt,
      };
    });
    // renew/close receive the raw private handle string, not marshalled JSON.
    const renew: RawHandler = async (err, handle) => {
      if (err) throw err;
      const refreshed = (def.renew && (await def.renew(handle))) || {};
      return JSON.stringify({ private: refreshed.private, renewAt: refreshed.renewAt });
    };
    const close: RawHandler = async (err, handle) => {
      if (err) throw err;
      if (def.close) await def.close(handle);
      return "null";
    };
    this.raw.ephemeral(
      typeName,
      schemaJson(def.schema, def),
      open,
      renew,
      close,
      validateAdapter(def.validate),
    );
    return this;
  }

  /**
   * Serve the provider over the Terraform plugin protocol. Resolves only when
   * the host shuts the provider down (SIGTERM), so `await` it to stay running.
   */
  serve(): Promise<void> {
    return this.raw.serve();
  }
}
