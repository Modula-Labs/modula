// Types for modula/iac: .modula/modula.ts (Modula Deploy config as code).

/** What the CLI passes to the config function, once per environment. */
export interface ModulaContext {
  /** The environment being planned (lowercase); undefined for `config validate`. */
  readonly environment: string | undefined;
  /** Whether the environment is one of `names` (case-insensitive). */
  isEnvironment(...names: string[]): boolean;
  /** Whether the environment is a pull request preview. */
  readonly isPreview: boolean;
}

declare const kind: unique symbol;

export interface Service {
  readonly [kind]: "service";
  /** The service's name, as a slug ("Anton API" -> "anton-api"). */
  readonly name: string;
}

export interface Project {
  readonly [kind]: "project";
}

export interface Preserved {
  readonly [kind]: "preserve";
}

export interface Secret {
  readonly [kind]: "secret";
}

export type RepoSource = { repo: string; branch?: string; rootDir?: string };
export type ImageSource = { image: string };
export type LocalSource = { local: string };
export type Source = RepoSource | ImageSource | LocalSource;

/** A variable: a value (references like ${{postgres.DATABASE_URL}} are kept),
 * secret() (value kept out of the file) or preserve() (left as it is). */
export type Variable = string | number | boolean | Secret | Preserved | undefined | null;

/** A command: a string (split on spaces; `sh -c` when it uses shell syntax) or argv. */
export type Command = string | string[];

export type RestartPolicy = "on_failure" | "always" | "never" | "ON_FAILURE" | "ALWAYS" | "NEVER";
export type Builder = "railpack" | "dockerfile" | "RAILPACK" | "DOCKERFILE";
export type Domain = string | { host: string; port?: number };

/** Service settings. Aliases (Railway names) are listed next to each. */
export interface Settings {
  /** Alias: start. */
  startCommand?: Command;
  start?: Command;
  /** Alias: preDeploy. */
  preDeployCommand?: Command;
  preDeploy?: Command;
  port?: number;
  /** Alias: healthcheck. */
  healthcheckPath?: string;
  healthcheck?: string;
  /** Seconds. Alias: healthcheckTimeout. */
  healthcheckTimeoutSecs?: number;
  healthcheckTimeout?: number;
  /** Alias: restartPolicyType. */
  restartPolicy?: RestartPolicy;
  restartPolicyType?: RestartPolicy;
  restartPolicyMaxRetries?: number;
  /** Seconds the old deployment keeps running after the new one is live. */
  drainingSeconds?: number;
  /** Alias: memory. */
  memoryMb?: number;
  memory?: number;
  /** Thousandths of a CPU. Alias: cpu. */
  cpuMillis?: number;
  cpu?: number;
  /** Replicas per server. Alias: numReplicas. */
  replicas?: number;
  numReplicas?: number;
  builder?: Builder;
  dockerfilePath?: string;
  buildCommand?: string;
  autoDeploy?: boolean;
  /** Alias: watchPatterns. */
  watchPaths?: string[];
  watchPatterns?: string[];
  waitForCi?: boolean;
  /** 5-field cron (UTC). Alias: cron. */
  cronSchedule?: string;
  cron?: string;
  /** Alias: cronTimeout. */
  cronTimeoutSecs?: number;
  cronTimeout?: number;
  /** Scale to zero when idle ({} = after 10 minutes). */
  serverless?: { idleMinutes?: number };
  /** Railway's name for serverless: true = serverless {}. */
  sleepApplication?: boolean;
}

export interface ServiceOptions extends Settings {
  source?: Source;
  /** Railway-style groups; their keys are merged into the service. */
  deploy?: Settings;
  build?: Settings;
  /** Custom domains; ones not listed are deleted. */
  domains?: Domain[];
  /** Make sure the service has a generated domain. */
  generateDomain?: boolean;
  /** The volume's mount path, e.g. "/data". */
  volume?: string;
  /** Expose this port through the TCP proxy. */
  tcpProxy?: number;
  variables?: Record<string, Variable>;
  /** Server-only variables apply must not delete (or use preserve()). */
  preserve?: string[];
}

export type Template = "postgres" | "mysql" | "redis" | "mongo" | "bucket" | "r2";

export interface ProjectOptions {
  /** The project's services. */
  resources?: Array<Service | false | null | undefined>;
  /** Same as resources. */
  services?: Array<Service | false | null | undefined>;
  /** Shared variables (${{shared.NAME}}). */
  variables?: Record<string, Variable>;
  /** Shared server-only variables apply must not delete. */
  preserve?: string[];
  /** This file describes only some of the project's services. */
  partial?: boolean;
}

export type ModulaConfig = Project | ((ctx: ModulaContext) => Project | Promise<Project>);

export function defineModula(config: ModulaConfig): ModulaConfig;
export function project(name: string | undefined, options?: ProjectOptions): Project;
export function service(name: string, options?: ServiceOptions): Service;
export function database(template: Template, name?: string, options?: ServiceOptions): Service;
export function database(template: Template, options: ServiceOptions): Service;
export function postgres(name?: string, options?: ServiceOptions): Service;
export function mysql(name?: string, options?: ServiceOptions): Service;
export function redis(name?: string, options?: ServiceOptions): Service;
export function mongo(name?: string, options?: ServiceOptions): Service;
export function bucket(name?: string, options?: ServiceOptions): Service;
export function r2(name?: string, options?: ServiceOptions): Service;
export function github(repo: string, options?: { branch?: string; rootDir?: string }): RepoSource;
export function image(ref: string): ImageSource;
export function local(path?: string): LocalSource;
export function preserve(): Preserved;
export function secret(options?: { sealed?: boolean }): Secret;
/** ${{service.VAR}} (the service name is slugged; "shared" for shared variables). */
export function ref(service: string, variable: string): string;
/** The slug a service name becomes. */
export function slug(name: string): string;
/** How a command string becomes argv. */
export function command(value: Command): string[];
