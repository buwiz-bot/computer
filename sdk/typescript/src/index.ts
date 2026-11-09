/**
 * Connect-only buwiz-computer client.
 *
 * Pass the published exec URL, host URL, and token. This package never starts
 * Docker and never treats `/v1/info` advertised URLs as the dial address.
 */

export class BuwizComputerError extends Error {
  readonly status: number;
  readonly body: unknown;

  constructor(message: string, status: number, body: unknown) {
    super(message);
    this.name = "BuwizComputerError";
    this.status = status;
    this.body = body;
  }
}

function trimSlash(url: string): string {
  return url.trim().replace(/\/+$/, "");
}

async function parseBody(response: Response): Promise<unknown> {
  const text = await response.text();
  if (!text) {
    return null;
  }
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return { raw: text };
  }
}

export type ExecRequest = {
  command: string[] | string;
  cwd?: string;
  timeout_ms?: number;
  env?: Record<string, string>;
  stdin?: string | null;
  detach?: boolean;
  /** Deferred: guest returns 400. Use execStream(). */
  pty?: boolean;
};

export type ExecResponse = {
  stdout: string;
  stderr: string;
  exit_code: number | null;
  timed_out: boolean;
  duration_ms: number;
  truncated: boolean;
  /**
   * Both pipes reached EOF. `false` means a process the command left running
   * still holds them, so output written after the foreground exited was not
   * captured. Absent on guests older than this field.
   */
  output_complete?: boolean;
  cwd: string;
  exec_id?: string;
  detached?: boolean;
  status?: string;
};

export type ExecCancelResponse = {
  exec_id: string;
  cancelled: boolean;
};

export type FilePutRequest = {
  path: string;
  content: string;
  encoding?: string;
  create_dirs?: boolean;
};

export type ScreenshotResponse = {
  encoding: string;
  mime: string;
  width: number;
  height: number;
  png_base64?: string;
  bytes: number;
  path?: string;
};

export type CuaOk = { ok: boolean };

export type RecipeScreenshot = "none" | "end" | "each";

export type RecipeStep =
  | { op: "click"; x: number; y: number; button?: number }
  | { op: "double_click" | "double-click"; x: number; y: number; button?: number }
  | { op: "move"; x: number; y: number }
  | { op: "drag"; x1: number; y1: number; x2: number; y2: number; button?: number }
  | { op: "press" | "mousedown"; x: number; y: number; button?: number }
  | {
      op: "release" | "mouseup";
      x?: number;
      y?: number;
      button?: number;
      path?: { x: number; y: number }[];
    }
  | { op: "type"; text: string }
  | { op: "key"; key: string; action?: "tap" | "down" | "up" }
  | { op: "scroll"; x: number; y: number; dx: number; dy: number }
  | { op: "wait"; ms: number }
  | { op: "screenshot" }
  | { op: "reset_desktop" | "reset" };

export type RecipeSettle = "off" | "compressed" | "raw";

/**
 * What the receipt reports about the desktop the steps ran against.
 * `off` (default) adds nothing and costs nothing. `input` adds the window
 * under each pointer step and the keyboard focus before each type/key.
 * `page` adds the Chromium page URL either side of click/type/key.
 */
export type RecipeObserve = "off" | "input" | "page";

export type RecipeRequest = {
  name?: string;
  stop_on_error?: boolean;
  screenshot?: RecipeScreenshot;
  record?: boolean;
  artifact_dir?: string;
  settle?: RecipeSettle;
  observe?: RecipeObserve;
  steps: RecipeStep[];
};

export type RecipeArtifact = {
  kind: string;
  label: string;
  path: string;
  mime: string;
  bytes: number;
  width?: number;
  height?: number;
  step_index?: number;
};

/** A window as the guest found it. `id` is the form `/v1/desktop/windows` uses. */
export type ObservedWindow = {
  id: string;
  /** `WM_CLASS` as `instance.class`. */
  class?: string;
  /** `_NET_WM_NAME`, falling back to `WM_NAME`. For Chromium, the page title. */
  title?: string;
};

/**
 * `none` means no window held the keyboard focus, so the X server discarded
 * the keystrokes: the `type` reached nothing at all. `root` is the window
 * manager parking focus where nothing on this desktop listens.
 */
export type FocusState = "none" | "pointer_root" | "root" | "window";

/**
 * What the guest saw around one step.
 *
 * A missing field means the guest looked and got no answer — never that
 * nothing was there. A missing `observed` on the step means it did not look.
 */
export type StepObservation = {
  /** The window at the step's target coordinate, read before the step ran. */
  target?: ObservedWindow;
  /** Where the keys were about to go. `type` and `key` only. */
  focus?: { state: FocusState; window?: ObservedWindow };
  url_before?: string;
  /** Read as soon as the step returned; a navigation is not instant. */
  url_after?: string;
  /** Time spent looking rather than acting. Not included in the step's `ms`. */
  observe_ms: number;
};

export type RecipeStepResult = {
  index: number;
  op: string;
  /** No error was returned. Not a claim the step achieved anything. */
  ok: boolean;
  ms: number;
  error?: string;
  screenshot?: ScreenshotResponse;
  /** Present only when `observe` asked and the step had something to see. */
  observed?: StepObservation;
};

export type RecipeResponse = {
  /** No step returned an error. See `steps[].observed` for what was seen. */
  ok: boolean;
  name?: string;
  /**
   * Echoed from the request, absent when it was `off`. Without it, a receipt
   * with no `observed` blocks could not be told apart from one that never
   * asked for any.
   */
  observe?: Exclude<RecipeObserve, "off">;
  ran: number;
  stopped_at?: number;
  duration_ms: number;
  steps: RecipeStepResult[];
  screenshot?: ScreenshotResponse;
  artifacts?: RecipeArtifact[];
  recording_error?: string;
};

/** `POST /v1/cua/tree`. Every field is optional; `{}` is the focused window with the defaults. */
export type A11yTreeRequest = {
  /** `focused_window` (default): the window with the keyboard focus. `desktop`: every application. */
  scope?: "focused_window" | "desktop";
  /** Levels below the top nodes, 0–40 (default 12). */
  max_depth?: number;
  /** 1–2000 (default 400). */
  max_nodes?: number;
  /** The walk's budget, 1–10000 ms (default 3000). What's read by then is returned. */
  timeout_ms?: number;
  /** Also nodes that are neither showing nor visible, and what's under them. */
  include_hidden?: boolean;
  /** Nameless containers with nothing to act on give their place to their children (default true). */
  compact?: boolean;
};

/** Screen pixels, the space `click` takes. */
export type A11yBounds = { x: number; y: number; width: number; height: number };

export type A11yNode = {
  /** `<snapshot>:<n>`, for `cuaElement`; good for this tree only. */
  ref: string;
  /** AT-SPI's role in snake case: `push_button`, `entry`, `frame`, `page_tab`, … */
  role: string;
  /** May be empty. */
  name: string;
  description?: string;
  /** A text node's contents when they aren't its name. Never a password field's. */
  text?: string;
  /** `focused`, `enabled`, `checked`, `selected`, `editable`, … */
  states: string[];
  bounds?: A11yBounds;
  /** AT-SPI action names. Chromium's are unnamed and not listed; `press` still runs them. */
  actions?: string[];
  children?: A11yNode[];
  /** Children a limit left out. */
  children_omitted?: number;
};

export type A11yTreeResponse = {
  /** This tree's number. Only a screen's latest tree can be acted on. */
  snapshot: number;
  display: string;
  scope: "focused_window" | "desktop";
  nodes: A11yNode[];
  count: number;
  truncated: boolean;
  truncated_by: ("max_nodes" | "max_depth" | "time")[];
  /** Nodes whose application failed to answer, left out with what's under them. */
  skipped: number;
  /** Why `nodes` is empty, when it is. */
  reason?: string;
  elapsed_ms: number;
};

/**
 * `press`: the element's own AT-SPI action (the app says it took it, not that anything happened).
 * `click`: a real click in the middle of the element, where it is now. `focus`: the keyboard
 * focus. `set_text`: replace an editable field's text.
 */
export type A11yElementAction = "press" | "click" | "focus" | "set_text";

export type A11yElementResponse = {
  ok: boolean;
  ref: string;
  action: A11yElementAction;
  /** For `press`: the AT-SPI action run. */
  performed?: string;
  /** For `click`: where the click went. */
  at?: { x: number; y: number };
};

/** Another of the box's screens (`:N`) for a call; without it, the box's own. */
function onScreen(url: string, display?: string): string {
  return display ? `${url}?display=${encodeURIComponent(display)}` : url;
}

export class BuwizComputer {
  private constructor(
    readonly execUrl: string,
    readonly hostUrl: string,
    private readonly token: string,
  ) {}

  static connect(execUrl: string, hostUrl: string, token: string): BuwizComputer {
    const exec = trimSlash(execUrl);
    const host = trimSlash(hostUrl);
    if (!exec || !host || !token) {
      throw new Error("execUrl, hostUrl, and token are required");
    }
    return new BuwizComputer(exec, host, token);
  }

  async healthExec(): Promise<unknown> {
    return this.publicJson(`${this.execUrl}/v1/health`);
  }

  async healthHost(): Promise<unknown> {
    return this.publicJson(`${this.hostUrl}/v1/health`);
  }

  async ready(): Promise<unknown> {
    return this.authJson("GET", `${this.hostUrl}/v1/ready`);
  }

  /** Inventory only. Do not dial `endpoints` from this payload. */
  async info(): Promise<unknown> {
    return this.authJson("GET", `${this.hostUrl}/v1/info`);
  }

  async exec(request: ExecRequest, timeoutMs = 120_000): Promise<ExecResponse> {
    return this.authJson("POST", `${this.execUrl}/v1/exec`, request, timeoutMs);
  }

  async execStream(request: ExecRequest, timeoutMs = 120_000): Promise<string> {
    const response = await this.raw("POST", `${this.execUrl}/v1/exec/stream`, {
      headers: {
        authorization: `Bearer ${this.token}`,
        accept: "application/x-ndjson",
        "content-type": "application/json",
      },
      body: JSON.stringify(request),
      timeoutMs,
    });
    const text = await response.text();
    if (!response.ok) {
      throw new BuwizComputerError(`exec stream returned ${response.status}`, response.status, text);
    }
    return text;
  }

  async execStatus(id: string): Promise<ExecResponse> {
    return this.authJson("GET", `${this.execUrl}/v1/exec/${encodeURIComponent(id)}`);
  }

  /** Stop a running exec and its whole process group. */
  async execCancel(id: string): Promise<ExecCancelResponse> {
    return this.authJson("DELETE", `${this.execUrl}/v1/exec/${encodeURIComponent(id)}`);
  }

  async filesGet(path: string, encoding?: string): Promise<unknown> {
    const query = new URLSearchParams({ path });
    if (encoding) {
      query.set("encoding", encoding);
    }
    return this.authJson("GET", `${this.execUrl}/v1/files?${query.toString()}`);
  }

  async filesPut(request: FilePutRequest): Promise<{ path: string; bytes_written: number }> {
    return this.authJson("PUT", `${this.execUrl}/v1/files`, request);
  }

  async filesDelete(
    path: string,
    recursive = false,
  ): Promise<{ path: string; deleted: boolean }> {
    const query = new URLSearchParams({ path });
    if (recursive) {
      query.set("recursive", "true");
    }
    return this.authJson("DELETE", `${this.execUrl}/v1/files?${query.toString()}`);
  }

  async filesMkdir(
    path: string,
    parents = true,
  ): Promise<{ path: string; created: boolean }> {
    return this.authJson("POST", `${this.execUrl}/v1/files/mkdir`, { path, parents });
  }

  async filesRename(from: string, to: string): Promise<{ from: string; to: string }> {
    return this.authJson("POST", `${this.execUrl}/v1/files/rename`, { from, to });
  }

  async filesGetRaw(path: string): Promise<Uint8Array> {
    const query = new URLSearchParams({ path });
    const response = await this.raw("GET", `${this.execUrl}/v1/files/raw?${query.toString()}`, {
      headers: { accept: "application/octet-stream" },
    });
    if (!response.ok) {
      const body = await parseBody(response);
      throw new BuwizComputerError(`raw GET returned ${response.status}`, response.status, body);
    }
    return new Uint8Array(await response.arrayBuffer());
  }

  async filesPutRaw(path: string, body: Uint8Array | ArrayBuffer): Promise<{ path: string; bytes_written: number }> {
    const query = new URLSearchParams({ path });
    const response = await this.raw("PUT", `${this.execUrl}/v1/files/raw?${query.toString()}`, {
      headers: { "content-type": "application/octet-stream", accept: "application/json" },
      body: body instanceof Uint8Array ? body : new Uint8Array(body),
    });
    const parsed = await parseBody(response);
    if (!response.ok) {
      throw new BuwizComputerError(`raw PUT returned ${response.status}`, response.status, parsed);
    }
    return parsed as { path: string; bytes_written: number };
  }

  async desktop(): Promise<unknown> {
    return this.authJson("GET", `${this.hostUrl}/v1/desktop`);
  }

  /** Every screen of the box with its display, viewer port and state. */
  async screens(): Promise<unknown> {
    return this.authJson("GET", `${this.hostUrl}/v1/screens`);
  }

  /** When the screen settles or changes: voxl's `/wait` fields, times in ms. */
  async cuaWait(request: {
    idle?: number;
    change?: boolean;
    since?: number;
    region?: string;
    timeout?: number;
  } = {}): Promise<{ changed: boolean; timed_out: boolean; seq: number; waited_ms: number }> {
    return this.authJson(
      "POST",
      `${this.execUrl}/v1/cua/wait`,
      request,
      (request.timeout ?? 10_000) + 5_000,
    );
  }

  async recordStart(): Promise<{ id: number }> {
    return this.authJson("POST", `${this.execUrl}/v1/cua/record/start`);
  }

  /** Saves the MP4 under the workspace; `path` works with the file calls. */
  async recordStop(id: number): Promise<{ path: string; bytes: number }> {
    return this.authJson("POST", `${this.execUrl}/v1/cua/record/stop`, { id }, 60_000);
  }

  async chrome(): Promise<unknown> {
    return this.authJson("GET", `${this.hostUrl}/v1/chrome`);
  }

  async egress(): Promise<unknown> {
    return this.authJson("GET", `${this.hostUrl}/v1/egress`);
  }

  async windows(): Promise<unknown> {
    return this.authJson("GET", `${this.hostUrl}/v1/desktop/windows`);
  }

  async busy(): Promise<unknown> {
    return this.authJson("GET", `${this.execUrl}/v1/busy`);
  }

  async metrics(): Promise<unknown> {
    return this.authJson("GET", `${this.execUrl}/v1/metrics`);
  }

  async shutdown(target: "exec" | "host" = "exec"): Promise<unknown> {
    const base = target === "host" ? this.hostUrl : this.execUrl;
    return this.authJson("POST", `${base}/v1/shutdown`);
  }

  async screenshot(): Promise<ScreenshotResponse> {
    return this.authJson("POST", `${this.execUrl}/v1/cua/screenshot`, undefined, 60_000);
  }

  async screenshotPng(): Promise<Uint8Array> {
    const response = await this.raw("POST", `${this.execUrl}/v1/cua/screenshot?format=png`, {
      headers: { accept: "image/png" },
      timeoutMs: 60_000,
    });
    if (!response.ok) {
      const body = await parseBody(response);
      throw new BuwizComputerError(`screenshot PNG returned ${response.status}`, response.status, body);
    }
    return new Uint8Array(await response.arrayBuffer());
  }

  async click(x: number, y: number, button?: number): Promise<CuaOk> {
    return this.authJson("POST", `${this.execUrl}/v1/cua/click`, { x, y, button });
  }

  async doubleClick(x: number, y: number, button?: number): Promise<CuaOk> {
    return this.authJson("POST", `${this.execUrl}/v1/cua/double-click`, { x, y, button });
  }

  async move(x: number, y: number): Promise<CuaOk> {
    return this.authJson("POST", `${this.execUrl}/v1/cua/move`, { x, y });
  }

  async drag(
    x1: number,
    y1: number,
    x2: number,
    y2: number,
    button?: number,
  ): Promise<CuaOk> {
    return this.authJson("POST", `${this.execUrl}/v1/cua/drag`, { x1, y1, x2, y2, button });
  }

  async press(x: number, y: number, button?: number): Promise<CuaOk> {
    return this.authJson("POST", `${this.execUrl}/v1/cua/press`, { x, y, button });
  }

  async release(
    x?: number,
    y?: number,
    button?: number,
    path?: { x: number; y: number }[],
  ): Promise<CuaOk> {
    return this.authJson("POST", `${this.execUrl}/v1/cua/release`, { x, y, button, path });
  }

  async type(text: string): Promise<CuaOk> {
    return this.authJson("POST", `${this.execUrl}/v1/cua/type`, { text });
  }

  async key(key: string): Promise<CuaOk> {
    return this.authJson("POST", `${this.execUrl}/v1/cua/key`, { key });
  }

  async scroll(x: number, y: number, dx: number, dy: number): Promise<CuaOk> {
    return this.authJson("POST", `${this.execUrl}/v1/cua/scroll`, { x, y, dx, dy });
  }

  /** Many CUA steps in one request. The guest lints the plan before actuating. */
  async recipe(request: RecipeRequest): Promise<RecipeResponse> {
    return this.authJson("POST", `${this.execUrl}/v1/cua/recipe`, request, 120_000);
  }

  /**
   * The screen's accessibility tree (AT-SPI): native apps' and Chromium's buttons, fields and
   * tabs as text, with bounds in screen pixels. Bounded, and never an error for being cut.
   */
  async cuaTree(request: A11yTreeRequest = {}, display?: string): Promise<A11yTreeResponse> {
    const budget = request.timeout_ms ?? 3_000;
    return this.authJson(
      "POST",
      onScreen(`${this.execUrl}/v1/cua/tree`, display),
      request,
      budget + 10_000,
    );
  }

  /**
   * An action on a node of the screen's latest tree, by `ref`. A ref from an older tree is
   * refused with `409 stale_ref`: read the tree again.
   */
  async cuaElement(
    ref: string,
    action: A11yElementAction,
    text?: string,
    display?: string,
  ): Promise<A11yElementResponse> {
    return this.authJson("POST", onScreen(`${this.execUrl}/v1/cua/element`, display), {
      ref,
      action,
      text,
    });
  }

  private async publicJson(url: string): Promise<unknown> {
    const response = await fetch(url, { cache: "no-store" });
    const body = await parseBody(response);
    if (!response.ok) {
      throw new BuwizComputerError(`${url} returned ${response.status}`, response.status, body);
    }
    return body;
  }

  private async authJson<T>(
    method: string,
    url: string,
    body?: unknown,
    timeoutMs = 30_000,
  ): Promise<T> {
    const headers: Record<string, string> = {
      authorization: `Bearer ${this.token}`,
      accept: "application/json",
      "x-request-id": crypto.randomUUID?.() ?? `ts-${Date.now()}`,
    };
    const init: RequestInit = { method, headers, cache: "no-store" };
    if (body !== undefined) {
      headers["content-type"] = "application/json";
      init.body = JSON.stringify(body);
    }
    const response = await this.raw(method, url, {
      headers,
      body: init.body ?? undefined,
      timeoutMs,
    });
    const parsed = await parseBody(response);
    if (!response.ok) {
      throw new BuwizComputerError(`${url} returned ${response.status}`, response.status, parsed);
    }
    return parsed as T;
  }

  private async raw(
    method: string,
    url: string,
    init: { headers?: Record<string, string>; body?: BodyInit; timeoutMs?: number },
  ): Promise<Response> {
    const headers = new Headers(init.headers);
    if (!headers.has("authorization")) {
      headers.set("authorization", `Bearer ${this.token}`);
    }
    if (!headers.has("x-request-id")) {
      headers.set("x-request-id", crypto.randomUUID?.() ?? `ts-${Date.now()}`);
    }
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), init.timeoutMs ?? 30_000);
    try {
      return await fetch(url, {
        method,
        headers,
        body: init.body,
        signal: controller.signal,
        cache: "no-store",
      });
    } finally {
      clearTimeout(timer);
    }
  }
}
