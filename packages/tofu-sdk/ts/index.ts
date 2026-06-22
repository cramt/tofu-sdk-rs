/**
 * `@tofu-sdk/core` — write Terraform/OpenTofu providers in TypeScript.
 *
 * Schemas are [Zod](https://zod.dev) objects, and **the type defines everything**:
 * structure, runtime validation, inferred handler types, *and* the Terraform
 * dispositions. Each disposition (`computed`, `forceNew`, `sensitive`,
 * `writeOnly`, `deprecated`, `block`, `set`) rides on its field via `.meta({ … })`
 * (typed, so a bad key is a compile error). "Order must not matter" is just a
 * `z.set` (an unordered cty `set`). The legacy out-of-band disposition *arrays*
 * still work and are merged in.
 *
 * ```ts
 * import { z } from "zod";
 * import { Provider } from "@tofu-sdk/core";
 *
 * const Bucket = z.object({
 *   name: z.string().meta({ forceNew: true }),
 *   arn: z.string().meta({ computed: true }),
 *   aliases: z.set(z.string()),            // unordered → cty set
 * });
 *
 * await new Provider()
 *   .resource("aws_s3_bucket", {
 *     schema: Bucket,
 *     async create(planned) { // planned.aliases is a Set<string>
 *       return { ...planned, arn: `arn:aws:s3:::${planned.name}` };
 *     },
 *   })
 *   .serve();
 * ```
 */

import { z } from "zod";

import {
  type AttributeJson,
  type CtyType,
  ctyFromZod,
  type Dispositions,
  fieldCty,
  type FieldName,
  functionSignatureJson,
  paramNames,
  reviveSets,
  schemaJson,
  searchKeysOf,
  setReplacer,
  type TfMeta,
  toWireJson,
} from "./schema";

export type { CtyType, Dispositions, FieldName, TfMeta } from "./schema";

// The native addon's generated loader, loaded at runtime and hand-typed (its
// auto-generated `.d.ts` is intentionally not imported).
type RawHandler = (err: Error | null, input: string) => Promise<string>;

interface RawProvider {
  config(schemaJson: string, configure: RawHandler, validate: RawHandler): void;
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
  function(name: string, signatureJson: string, call: RawHandler): void;
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

// Schema derivation (Zod model → cty type) lives in `./schema` — a pure module
// with no native dependency, so it can be unit-tested directly. It reads the Zod
// type via Zod 4 introspection (not `z.toJSONSchema`, which throws on the very
// constructs the design law uses: `z.set` and `.transform`).

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
 * The per-call handler context, the TypeScript analog of Rust's `&mut Ctx`. It
 * reaches the things that are *not* part of the typed model: success-path
 * warnings, per-resource private state, and cancellation. Handed to every
 * resource / data-source handler as the final argument; ignore it if unused.
 */
export interface HandlerCtx {
  /** The resource's stored private state from its previous operation, if any. */
  readonly private: string | undefined;
  /** Persist new private state for the resource's next operation. */
  setPrivate(value: string): void;
  /** Emit a warning that rides alongside a *successful* result. */
  warn(summary: string, detail?: string): void;
  /** Emit a prebuilt warning diagnostic (e.g. pointed at an attribute). */
  warning(diag: Diagnostic): void;
  /** Whether `StopProvider` had been received when the handler started. */
  readonly cancelled: boolean;
  /**
   * Aborts when the operation is cancelled — pass it to `fetch`/etc. to abort
   * promptly. Pre-aborted if the provider was already stopping at handler entry.
   */
  readonly signal: AbortSignal;
}

/**
 * A `validate` hook: inspect a config and return diagnostics (or nothing).
 * Runs before planning. Attributes the user did not set — or whose values are
 * not yet known (references to other resources) — arrive as `null`/`undefined`,
 * so guard before validating them.
 */
type Validate<C> = (config: C) => Diagnostic[] | void | Promise<Diagnostic[] | void>;

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
  create(planned: z.infer<S>, ctx: HandlerCtx): Promise<z.infer<S>>;
  /** Refresh state; return `null` if the resource no longer exists. */
  read?(current: z.infer<S>, ctx: HandlerCtx): Promise<z.infer<S> | null>;
  /** Update in place and return the new state. */
  update?(planned: z.infer<S>, prior: z.infer<S>, ctx: HandlerCtx): Promise<z.infer<S>>;
  /** Delete the resource. */
  delete?(prior: z.infer<S>, ctx: HandlerCtx): Promise<void>;
  /** Import an existing resource by ID, returning its state (then refreshed via `read`). */
  import?(id: string, ctx: HandlerCtx): Promise<z.infer<S>>;
  /**
   * Migrate stored state written at `fromVersion` to the current schema.
   * `prior` is the raw stored state (untyped — it predates the current schema).
   */
  upgrade?(fromVersion: number, prior: unknown, ctx: HandlerCtx): Promise<z.infer<S>>;
  /** Validate the configuration, returning diagnostics (or nothing). */
  validate?: Validate<z.infer<S>>;
}

/** A singular read-only data source: given a config, produce a state. */
export interface DataSource<S extends z.ZodObject<z.ZodRawShape>> extends Dispositions<S> {
  schema: S;
  read(config: z.infer<S>, ctx: HandlerCtx): Promise<z.infer<S>>;
  /** Validate the configuration, returning diagnostics (or nothing). */
  validate?: Validate<z.infer<S>>;
}

/**
 * A plural read-only data source: a lookup by search keys resolving to a
 * `results` list. `schema` describes one element; the query-input fields are the
 * ones tagged `.meta({ searchKey: true })` on the schema (preferred), or named in
 * the optional `searchKeys` array (merged in for back-compat). At least one is
 * required.
 */
export interface DataSourceList<S extends z.ZodObject<z.ZodRawShape>> {
  schema: S;
  /** Query-input field names. Optional — prefer `.meta({ searchKey: true })`. */
  searchKeys?: FieldName<S>[];
  list(query: z.infer<S>, ctx: HandlerCtx): Promise<z.infer<S>[]>;
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
  /** Validate the provider block, returning diagnostics. Runs before `configure`. */
  validate?: Validate<z.infer<S>>;
}

/**
 * A provider-defined function: pure, positional, called from HCL as
 * `provider::<provider>::<name>(…)`. `params` is an **object** schema whose key
 * order is the positional parameter order; `returns` is the result schema.
 */
export interface ProviderFunction<
  P extends z.ZodObject<z.ZodRawShape>,
  R extends z.ZodType,
> {
  params: P;
  returns: R;
  /** One-line summary, surfaced in docs. */
  summary?: string;
  /** Longer description. */
  description?: string;
  /** Compute the result from the named arguments. */
  call(args: z.infer<P>): Promise<z.infer<R>>;
}

/**
 * A **variadic** provider-defined function: fixed leading parameters plus zero or
 * more trailing arguments of a single uniform type. The 1-to-1 of Rust's
 * `VariadicFunction` (leading `Params` + a `VarArg` element type + `call(params,
 * rest)`); the type system enforces "exactly one uniform trailing variadic"
 * because `variadic` is a single schema and `rest` is its array.
 */
export interface VariadicFunction<
  P extends z.ZodObject<z.ZodRawShape>,
  V extends z.ZodType,
  R extends z.ZodType,
> {
  /** The fixed leading parameters (object key order = positional order). */
  params: P;
  /** The uniform element type of the trailing arguments. */
  variadic: V;
  returns: R;
  summary?: string;
  description?: string;
  /** Compute the result from the leading args and the variadic `rest`. */
  call(args: z.infer<P>, rest: z.infer<V>[]): Promise<z.infer<R>>;
}

/**
 * Adapt an async `A -> R` handler to the raw `(err, json) -> Promise<json>` form.
 * When a `schema` is given the parsed input is revived (JSON arrays backing
 * `z.set` fields become JS `Set`s, matching `z.infer`), and the result is encoded
 * with {@link toWireJson} (any `Set` → its array wire form).
 */
function adapt<A, R>(fn: (arg: A) => Promise<R>, schema?: z.ZodType): RawHandler {
  return async (err, input) => {
    if (err) throw err;
    const parsed = JSON.parse(input);
    const arg = (schema ? reviveSets<A>(schema, parsed) : (parsed as A)) as A;
    const result = await fn(arg);
    return toWireJson(result);
  };
}

/** The ctx data the addon threads in (`{ private, cancelled }`). */
interface CtxIn {
  private?: string | null;
  cancelled?: boolean;
}

/** A live [`HandlerCtx`] plus a private accessor to drain its side effects. */
interface DrainableCtx extends HandlerCtx {
  drain(): { private?: string; warnings: Diagnostic[] };
}

/** Build a [`HandlerCtx`] from the addon's incoming ctx data, collecting the
 * handler's `setPrivate`/`warn` calls to thread back out. */
function makeCtx(input?: CtxIn): DrainableCtx {
  const warnings: Diagnostic[] = [];
  let outPrivate: string | undefined;
  const controller = new AbortController();
  const cancelled = input?.cancelled === true;
  if (cancelled) controller.abort();
  return {
    private: input?.private ?? undefined,
    cancelled,
    signal: controller.signal,
    setPrivate(value) {
      outPrivate = value;
    },
    warn(summary, detail) {
      warnings.push({ severity: "warning", summary, detail });
    },
    warning(diag) {
      warnings.push({ ...diag, severity: diag.severity ?? "warning" });
    },
    drain() {
      return { private: outPrivate, warnings };
    },
  };
}

/**
 * Adapt a ctx-threaded handler to the raw form: parse the `{ value, ctx }`
 * envelope the addon sends, build a {@link HandlerCtx}, run the handler, and
 * return `{ value, ctx: { private, warnings } }`. `reviveSchema` (when given)
 * revives `z.set` arrays in the value before the handler sees it.
 */
function ctxAdapt<V, R>(
  reviveSchema: z.ZodType | undefined,
  fn: (value: V, ctx: HandlerCtx) => Promise<R>,
): RawHandler {
  return async (err, input) => {
    if (err) throw err;
    const env = JSON.parse(input) as { value: unknown; ctx?: CtxIn };
    const value = (reviveSchema ? reviveSets<V>(reviveSchema, env.value) : (env.value as V)) as V;
    const ctx = makeCtx(env.ctx);
    const result = await fn(value, ctx);
    const drained = ctx.drain();
    return JSON.stringify(
      { value: result ?? null, ctx: { private: drained.private, warnings: drained.warnings } },
      setReplacer,
    );
  };
}

/** Adapt a `validate` hook to the raw form, returning a JSON diagnostics array. */
function validateAdapter<C>(validate: Validate<C> | undefined, schema?: z.ZodType): RawHandler {
  return async (err, input) => {
    if (err) throw err;
    if (!validate) return "[]";
    const parsed = JSON.parse(input);
    const config = (schema ? reviveSets<C>(schema, parsed) : (parsed as C)) as C;
    const diagnostics = (await validate(config)) ?? [];
    return JSON.stringify(diagnostics);
  };
}

/** A Terraform/OpenTofu provider authored in TypeScript. */
export class Provider {
  private readonly raw: RawProvider = new native.Provider();

  /** Declare the provider's configuration block, its `configure` handler, and an
   * optional `validate` hook (run before configure). */
  config<S extends z.ZodObject<z.ZodRawShape>>(def: ProviderConfig<S>): this {
    this.raw.config(
      schemaJson(def.schema),
      adapt(async (cfg: z.infer<S>) => {
        await def.configure(cfg);
        return null;
      }, def.schema),
      validateAdapter(def.validate, def.schema),
    );
    return this;
  }

  /** Register a provider-defined function under `name`. */
  function<P extends z.ZodObject<z.ZodRawShape>, R extends z.ZodType>(
    name: string,
    def: ProviderFunction<P, R>,
  ): this {
    const names = paramNames(def.params);
    const call: RawHandler = async (err, input) => {
      if (err) throw err;
      // The addon delivers the (already cty-decoded) arguments positionally.
      const argv = JSON.parse(input) as unknown[];
      const obj: Record<string, unknown> = {};
      names.forEach((n, i) => (obj[n] = argv[i]));
      const args = reviveSets<z.infer<P>>(def.params, obj);
      const result = await def.call(args);
      return toWireJson(validateOut(def.returns, result, `function ${name}`));
    };
    this.raw.function(
      name,
      functionSignatureJson(def.params, def.returns, {
        summary: def.summary,
        description: def.description,
      }),
      call,
    );
    return this;
  }

  /** Register a **variadic** provider-defined function under `name`: fixed leading
   * params (`params`) plus zero or more trailing args of `variadic`'s type. */
  functionVariadic<
    P extends z.ZodObject<z.ZodRawShape>,
    V extends z.ZodType,
    R extends z.ZodType,
  >(name: string, def: VariadicFunction<P, V, R>): this {
    const names = paramNames(def.params);
    const call: RawHandler = async (err, input) => {
      if (err) throw err;
      // Positional args: the first `names.length` are the fixed params; the rest
      // are the variadic tail (all of `variadic`'s type).
      const argv = JSON.parse(input) as unknown[];
      const obj: Record<string, unknown> = {};
      names.forEach((n, i) => (obj[n] = argv[i]));
      const args = reviveSets<z.infer<P>>(def.params, obj);
      const rest = argv.slice(names.length).map((v) => reviveSets<z.infer<V>>(def.variadic, v));
      const result = await def.call(args, rest);
      return toWireJson(validateOut(def.returns, result, `function ${name}`));
    };
    this.raw.function(
      name,
      functionSignatureJson(def.params, def.returns, {
        summary: def.summary,
        description: def.description,
        variadic: def.variadic,
      }),
      call,
    );
    return this;
  }

  /** Register a managed resource under `typeName`. */
  resource<S extends z.ZodObject<z.ZodRawShape>>(typeName: string, def: Resource<S>): this {
    type M = z.infer<S>;
    const create = ctxAdapt<M, M>(def.schema, (planned, ctx) =>
      def
        .create(planned, ctx)
        .then((s) => validateOut(def.schema, s, `resource ${typeName} create`)),
    );
    const read = ctxAdapt<M, M | null>(def.schema, async (current, ctx) => {
      if (!def.read) return current;
      const refreshed = await def.read(current, ctx);
      return refreshed === null
        ? null
        : validateOut(def.schema, refreshed, `resource ${typeName} read`);
    });
    const update = ctxAdapt<{ planned: unknown; prior: unknown }, M>(undefined, async (raw, ctx) => {
      if (!def.update) {
        throw new Error(`resource "${typeName}" does not support in-place update`);
      }
      const planned = reviveSets<M>(def.schema, raw.planned);
      const prior = reviveSets<M>(def.schema, raw.prior);
      return validateOut(def.schema, await def.update(planned, prior, ctx), `resource ${typeName} update`);
    });
    const del = ctxAdapt<M, null>(def.schema, async (prior, ctx) => {
      if (def.delete) await def.delete(prior, ctx);
      return null;
    });
    const imp = ctxAdapt<string, M>(undefined, async (id, ctx) => {
      if (!def.import) {
        throw new Error(`resource "${typeName}" does not support import`);
      }
      return validateOut(def.schema, await def.import(id, ctx), `resource ${typeName} import`);
    });
    const upgrade = ctxAdapt<{ fromVersion: number; priorState: unknown }, M>(
      undefined,
      async (raw, ctx) => {
        if (!def.upgrade) {
          throw new Error(
            `resource "${typeName}" has no state upgrade (stored version ${raw.fromVersion})`,
          );
        }
        return validateOut(
          def.schema,
          await def.upgrade(raw.fromVersion, raw.priorState, ctx),
          `resource ${typeName} upgrade`,
        );
      },
    );
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
      validateAdapter(def.validate, def.schema),
    );
    return this;
  }

  /** Register a singular read-only data source under `typeName`. */
  dataSource<S extends z.ZodObject<z.ZodRawShape>>(typeName: string, def: DataSource<S>): this {
    this.raw.dataSource(
      typeName,
      schemaJson(def.schema, def),
      ctxAdapt<z.infer<S>, z.infer<S>>(def.schema, (config, ctx) =>
        def
          .read(config, ctx)
          .then((s) => validateOut(def.schema, s, `data source ${typeName} read`)),
      ),
      validateAdapter(def.validate, def.schema),
    );
    return this;
  }

  /**
   * Register a plural read-only data source under `typeName`: the search keys
   * are query inputs and the result is a computed `results` list of objects
   * matching `schema`.
   */
  dataSourceList<S extends z.ZodObject<z.ZodRawShape>>(
    typeName: string,
    def: DataSourceList<S>,
  ): this {
    type M = z.infer<S>;
    const elementType = ctyFromZod(def.schema);
    // Query inputs: `.meta({ searchKey })` fields plus the legacy array, deduped.
    const searchKeys = [...new Set([...searchKeysOf(def.schema), ...(def.searchKeys ?? [])])];
    if (searchKeys.length === 0) {
      throw new Error(
        `data source list "${typeName}" needs at least one search key ` +
          "(tag a field `.meta({ searchKey: true })` or pass `searchKeys`)",
      );
    }
    // The wrapper block: the search keys as optional inputs, plus `results`.
    const attributes: AttributeJson[] = searchKeys.map((key) => ({
      name: key,
      type: fieldCty(def.schema, key),
      required: false,
      optional: true,
      computed: false,
      forceNew: false,
      sensitive: false,
      writeOnly: false,
      deprecated: false,
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
      deprecated: false,
    });

    const read = ctxAdapt<M, Record<string, unknown>>(def.schema, async (config, ctx) => {
      const items = await def.list(config, ctx);
      const validated = items.map((item, i) =>
        validateOut(def.schema, item, `data source ${typeName} list[${i}]`),
      );
      const inputs: Record<string, unknown> = {};
      for (const key of searchKeys) inputs[key] = (config as Record<string, unknown>)[key];
      return { ...inputs, results: validated };
    });
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
    }, def.schema);
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
      validateAdapter(def.validate, def.schema),
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
