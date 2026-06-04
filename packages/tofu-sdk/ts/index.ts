/**
 * `@tofu-sdk/core` — write Terraform/OpenTofu providers in TypeScript.
 *
 * You describe each resource/data source with a schema and async handlers; the
 * Rust core (loaded as a native addon) runs the real tfplugin6 server, performs
 * the go-plugin handshake, and drives your handlers with decoded values. No
 * protocol, gRPC, or msgpack to deal with.
 *
 * ```ts
 * import { Provider } from "@tofu-sdk/core";
 *
 * await new Provider()
 *   .resource("aws_s3_bucket", {
 *     schema: {
 *       name: { type: "string", required: true, forceNew: true },
 *       arn: { type: "string", computed: true },
 *     },
 *     async create(planned) {
 *       return { ...planned, arn: `arn:aws:s3:::${planned.name}` };
 *     },
 *   })
 *   .serve();
 * ```
 */

import { createRequire } from "node:module";
import { join } from "node:path";

// The native addon's generated loader. Loaded at runtime and hand-typed below;
// its auto-generated `.d.ts` is intentionally not imported so this module owns
// the public surface.
type RawHandler = (err: Error | null, input: string) => Promise<string>;

interface RawProvider {
  resource(
    typeName: string,
    schemaJson: string,
    create: RawHandler,
    read: RawHandler,
    update: RawHandler,
    del: RawHandler,
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

/**
 * A `cty` type constraint in its JSON form — the same shape Terraform uses:
 * `"string"`, `["list", "string"]`, `["object", { name: "string" }]`, …
 */
export type CtyType =
  | "string"
  | "number"
  | "bool"
  | "dynamic"
  | ["list", CtyType]
  | ["set", CtyType]
  | ["map", CtyType]
  | ["tuple", CtyType[]]
  | ["object", Record<string, CtyType>]
  | ["object", Record<string, CtyType>, string[]];

/** One attribute in a schema. */
export interface Attribute {
  /** The attribute's `cty` type. */
  type: CtyType;
  /** The caller must set this attribute. */
  required?: boolean;
  /** The caller may set this attribute. */
  optional?: boolean;
  /** The provider computes this attribute (unknown until applied). */
  computed?: boolean;
  /** Changing this attribute forces the resource to be replaced. */
  forceNew?: boolean;
  /** The value is sensitive and should be redacted. */
  sensitive?: boolean;
  /** Human-readable description. */
  description?: string;
}

/** A block's attributes, keyed by name. */
export type Schema = Record<string, Attribute>;

/**
 * A managed resource's lifecycle. `T` is the decoded model (an object matching
 * the schema). `create` is required; the rest default to sensible no-ops.
 */
export interface Resource<T> {
  schema: Schema;
  /** Create the resource and return its new state (computed fields filled). */
  create(planned: T): Promise<T>;
  /** Refresh state; return `null` if the resource no longer exists. */
  read?(current: T): Promise<T | null>;
  /** Update in place and return the new state. */
  update?(planned: T, prior: T): Promise<T>;
  /** Delete the resource. */
  delete?(prior: T): Promise<void>;
}

/** A read-only data source: given a config, produce a state. */
export interface DataSource<TConfig, TState = TConfig> {
  schema: Schema;
  read(config: TConfig): Promise<TState>;
}

function schemaToJson(schema: Schema): string {
  const attributes = Object.entries(schema).map(([name, a]) => ({
    name,
    type: a.type,
    required: a.required ?? false,
    optional: a.optional ?? false,
    computed: a.computed ?? false,
    forceNew: a.forceNew ?? false,
    sensitive: a.sensitive ?? false,
    description: a.description,
  }));
  return JSON.stringify({ attributes });
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

  /** Register a managed resource under `typeName`. */
  resource<T extends Record<string, unknown>>(
    typeName: string,
    def: Resource<T>,
  ): this {
    const create = adapt((planned: T) => def.create(planned));
    const read = adapt((current: T) =>
      def.read ? def.read(current) : Promise.resolve(current),
    );
    const update: RawHandler = async (err, input) => {
      if (err) throw err;
      const { planned, prior } = JSON.parse(input) as { planned: T; prior: T };
      if (!def.update) {
        throw new Error(`resource "${typeName}" does not support in-place update`);
      }
      return JSON.stringify(await def.update(planned, prior));
    };
    const del = adapt(async (prior: T) => {
      if (def.delete) await def.delete(prior);
      return null;
    });
    this.raw.resource(typeName, schemaToJson(def.schema), create, read, update, del);
    return this;
  }

  /** Register a read-only data source under `typeName`. */
  dataSource<TConfig extends Record<string, unknown>, TState>(
    typeName: string,
    def: DataSource<TConfig, TState>,
  ): this {
    this.raw.dataSource(
      typeName,
      schemaToJson(def.schema),
      adapt((config: TConfig) => def.read(config)),
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
