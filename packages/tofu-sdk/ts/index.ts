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

import { createRequire } from "node:module";
import { join } from "node:path";

import { z } from "zod";

// The native addon's generated loader, loaded at runtime and hand-typed (its
// auto-generated `.d.ts` is intentionally not imported).
type RawHandler = (err: Error | null, input: string) => Promise<string>;

interface RawProvider {
  config(schemaJson: string, configure: RawHandler): void;
  resource(
    typeName: string,
    schemaJson: string,
    create: RawHandler,
    read: RawHandler,
    update: RawHandler,
    del: RawHandler,
    imp: RawHandler,
  ): void;
  dataSource(typeName: string, schemaJson: string, read: RawHandler): void;
  serve(): Promise<void>;
}

interface RawBinding {
  Provider: new () => RawProvider;
}

const native = createRequire(__filename)(
  join(__dirname, "..", "binding", "index.js"),
) as RawBinding;

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
}

/** Build the `{ attributes: [...] }` schema JSON the native addon consumes. */
function schemaJson(
  schema: z.ZodObject<z.ZodRawShape>,
  dispositions: Dispositions<z.ZodObject<z.ZodRawShape>> = {},
): string {
  const json = z.toJSONSchema(schema) as JsonSchema;
  const required = new Set(json.required ?? []);
  const computed = new Set<string>(dispositions.computed ?? []);
  const forceNew = new Set<string>(dispositions.forceNew ?? []);
  const sensitive = new Set<string>(dispositions.sensitive ?? []);

  const attributes = Object.entries(json.properties ?? {}).map(([name, prop]) => {
    const isComputed = computed.has(name);
    const isRequired = required.has(name) && !isComputed;
    return {
      name,
      type: jsonSchemaToCty(prop),
      required: isRequired,
      optional: !isRequired && !isComputed,
      computed: isComputed,
      forceNew: forceNew.has(name),
      sensitive: sensitive.has(name),
    };
  });
  return JSON.stringify({ attributes });
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
}

/** A singular read-only data source: given a config, produce a state. */
export interface DataSource<S extends z.ZodObject<z.ZodRawShape>> extends Dispositions<S> {
  schema: S;
  read(config: z.infer<S>): Promise<z.infer<S>>;
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
    this.raw.resource(typeName, schemaJson(def.schema, def), create, read, update, del, imp);
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
    }));
    attributes.push({
      name: "results",
      type: ["list", elementType] as CtyType,
      required: false,
      optional: false,
      computed: true,
      forceNew: false,
      sensitive: false,
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
    this.raw.dataSource(typeName, JSON.stringify({ attributes }), read);
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
