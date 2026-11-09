"""Connect-only HTTP client for a running buwiz-computer guest."""

from __future__ import annotations

from typing import Any, Mapping
from urllib.parse import quote, urlencode

import httpx


class BuwizComputerError(Exception):
    def __init__(self, message: str, status: int, body: Any) -> None:
        super().__init__(message)
        self.status = status
        self.body = body


DEFAULT_TIMEOUT = 120.0
"""Seconds a request may take, unless it waits on purpose."""

BROWSER_MARGIN = 20.0
"""Seconds added to a browser wait's or run's own ``timeout_ms``: the guest gives the browser 15 s
past it, then replies."""


def _trim_slash(url: str) -> str:
    return url.strip().rstrip("/")


class BuwizComputer:
    def __init__(self, exec_url: str, host_url: str, token: str) -> None:
        self.exec_url = _trim_slash(exec_url)
        self.host_url = _trim_slash(host_url)
        self.token = token
        self._timeout = DEFAULT_TIMEOUT
        self._http = httpx.Client(timeout=DEFAULT_TIMEOUT)

    @classmethod
    def connect(cls, exec_url: str, host_url: str, token: str) -> BuwizComputer:
        if not exec_url or not host_url or not token:
            raise ValueError("exec_url, host_url, and token are required")
        return cls(exec_url, host_url, token)

    def close(self) -> None:
        self._http.close()

    def __enter__(self) -> BuwizComputer:
        return self

    def __exit__(self, *args: object) -> None:
        self.close()

    def health_exec(self) -> Any:
        return self._public(f"{self.exec_url}/v1/health")

    def health_host(self) -> Any:
        return self._public(f"{self.host_url}/v1/health")

    def ready(self) -> Any:
        return self._auth("GET", f"{self.host_url}/v1/ready")

    def info(self) -> Any:
        """Inventory only. Do not dial endpoints from this payload."""
        return self._auth("GET", f"{self.host_url}/v1/info")

    def exec(
        self,
        command: list[str] | str,
        cwd: str | None = None,
        timeout_ms: int | None = None,
        env: Mapping[str, str] | None = None,
        stdin: str | None = None,
        detach: bool | None = None,
    ) -> Any:
        body: dict[str, Any] = {"command": command}
        if cwd is not None:
            body["cwd"] = cwd
        if timeout_ms is not None:
            body["timeout_ms"] = timeout_ms
        if env is not None:
            body["env"] = dict(env)
        if stdin is not None:
            body["stdin"] = stdin
        if detach is not None:
            body["detach"] = detach
        return self._auth("POST", f"{self.exec_url}/v1/exec", body)

    def exec_status(self, exec_id: str) -> Any:
        return self._auth("GET", f"{self.exec_url}/v1/exec/{exec_id}")

    def exec_cancel(self, exec_id: str) -> Any:
        """Stop a running exec and its whole process group."""
        return self._auth("DELETE", f"{self.exec_url}/v1/exec/{exec_id}")

    def files_get(self, path: str, encoding: str | None = None) -> Any:
        query = {"path": path}
        if encoding:
            query["encoding"] = encoding
        return self._auth("GET", f"{self.exec_url}/v1/files?{urlencode(query)}")

    def files_put(
        self,
        path: str,
        content: str,
        encoding: str | None = None,
        create_dirs: bool = True,
    ) -> Any:
        body: dict[str, Any] = {
            "path": path,
            "content": content,
            "create_dirs": create_dirs,
        }
        if encoding:
            body["encoding"] = encoding
        return self._auth("PUT", f"{self.exec_url}/v1/files", body)

    def files_delete(self, path: str, recursive: bool = False) -> Any:
        query = {"path": path}
        if recursive:
            query["recursive"] = "true"
        return self._auth("DELETE", f"{self.exec_url}/v1/files?{urlencode(query)}")

    def files_mkdir(self, path: str, parents: bool = True) -> Any:
        return self._auth(
            "POST",
            f"{self.exec_url}/v1/files/mkdir",
            {"path": path, "parents": parents},
        )

    def files_rename(self, frm: str, to: str) -> Any:
        return self._auth("POST", f"{self.exec_url}/v1/files/rename", {"from": frm, "to": to})

    def files_get_raw(self, path: str) -> bytes:
        response = self._http.get(
            f"{self.exec_url}/v1/files/raw",
            params={"path": path},
            headers={
                "authorization": f"Bearer {self.token}",
                "accept": "application/octet-stream",
                "x-request-id": self._request_id(),
            },
        )
        if response.status_code >= 400:
            raise BuwizComputerError(
                f"raw GET returned {response.status_code}",
                response.status_code,
                self._body(response),
            )
        return response.content

    def files_put_raw(self, path: str, content: bytes) -> Any:
        response = self._http.put(
            f"{self.exec_url}/v1/files/raw",
            params={"path": path},
            headers={
                "authorization": f"Bearer {self.token}",
                "content-type": "application/octet-stream",
                "x-request-id": self._request_id(),
            },
            content=content,
        )
        return self._decode(response, f"{self.exec_url}/v1/files/raw")

    def desktop(self) -> Any:
        return self._auth("GET", f"{self.host_url}/v1/desktop")

    def screens(self) -> Any:
        """Every screen of the box with its display, viewer port and state."""
        return self._auth("GET", f"{self.host_url}/v1/screens")

    def cua_wait(self, **fields: Any) -> Any:
        """When the screen settles or changes: idle, change, since, region, timeout."""
        return self._auth("POST", f"{self.exec_url}/v1/cua/wait", fields)

    def record_start(self) -> Any:
        return self._auth("POST", f"{self.exec_url}/v1/cua/record/start")

    def record_stop(self, recording_id: int) -> Any:
        """Saves the MP4 under the workspace; `path` works with the file calls."""
        return self._auth("POST", f"{self.exec_url}/v1/cua/record/stop", {"id": recording_id})

    def chrome(self) -> Any:
        return self._auth("GET", f"{self.host_url}/v1/chrome")

    def active_tab(self, display: str | None = None) -> Any:
        """The page in front of Chromium, as the box's extension last reported it.

        `url` is None when nothing has. `display` (":2") asks for a Bot's own screen.
        """
        query = f"?display={quote(display, safe='')}" if display else ""
        return self._auth("GET", f"{self.host_url}/v1/chrome/active-tab{query}")

    def egress(self) -> Any:
        return self._auth("GET", f"{self.host_url}/v1/egress")

    def windows(self) -> Any:
        return self._auth("GET", f"{self.host_url}/v1/desktop/windows")

    def busy(self) -> Any:
        return self._auth("GET", f"{self.exec_url}/v1/busy")

    def metrics(self) -> Any:
        return self._auth("GET", f"{self.exec_url}/v1/metrics")

    def shutdown(self, host: bool = False) -> Any:
        base = self.host_url if host else self.exec_url
        return self._auth("POST", f"{base}/v1/shutdown")

    def exec_stream(self, command: list[str] | str, **kwargs: Any) -> str:
        body: dict[str, Any] = {"command": command, **kwargs}
        response = self._http.post(
            f"{self.exec_url}/v1/exec/stream",
            headers={
                "authorization": f"Bearer {self.token}",
                "accept": "application/x-ndjson",
                "x-request-id": self._request_id(),
            },
            json=body,
        )
        if response.status_code >= 400:
            raise BuwizComputerError(
                f"exec stream returned {response.status_code}",
                response.status_code,
                self._body(response),
            )
        return response.text

    def screenshot(self) -> Any:
        return self._auth("POST", f"{self.exec_url}/v1/cua/screenshot")

    def screenshot_png(self) -> bytes:
        response = self._http.post(
            f"{self.exec_url}/v1/cua/screenshot",
            params={"format": "png"},
            headers={
                "authorization": f"Bearer {self.token}",
                "accept": "image/png",
            },
        )
        if response.status_code >= 400:
            body: Any
            try:
                body = response.json()
            except Exception:
                body = {"raw": response.text}
            raise BuwizComputerError(
                f"screenshot PNG returned {response.status_code}",
                response.status_code,
                body,
            )
        return response.content

    def click(self, x: int, y: int, button: int | None = None) -> Any:
        return self._auth("POST", f"{self.exec_url}/v1/cua/click", {"x": x, "y": y, "button": button})

    def double_click(self, x: int, y: int, button: int | None = None) -> Any:
        return self._auth(
            "POST",
            f"{self.exec_url}/v1/cua/double-click",
            {"x": x, "y": y, "button": button},
        )

    def move(self, x: int, y: int) -> Any:
        return self._auth("POST", f"{self.exec_url}/v1/cua/move", {"x": x, "y": y})

    def drag(
        self,
        x1: int,
        y1: int,
        x2: int,
        y2: int,
        button: int | None = None,
    ) -> Any:
        return self._auth(
            "POST",
            f"{self.exec_url}/v1/cua/drag",
            {"x1": x1, "y1": y1, "x2": x2, "y2": y2, "button": button},
        )

    def press(self, x: int, y: int, button: int | None = None) -> Any:
        return self._auth("POST", f"{self.exec_url}/v1/cua/press", {"x": x, "y": y, "button": button})

    def release(
        self,
        x: int | None = None,
        y: int | None = None,
        button: int | None = None,
        path: list[dict[str, int]] | None = None,
    ) -> Any:
        return self._auth(
            "POST",
            f"{self.exec_url}/v1/cua/release",
            {"x": x, "y": y, "button": button, "path": path},
        )

    def type(self, text: str) -> Any:
        return self._auth("POST", f"{self.exec_url}/v1/cua/type", {"text": text})

    def key(self, key: str) -> Any:
        return self._auth("POST", f"{self.exec_url}/v1/cua/key", {"key": key})

    def scroll(self, x: int, y: int, dx: int, dy: int) -> Any:
        return self._auth(
            "POST",
            f"{self.exec_url}/v1/cua/scroll",
            {"x": x, "y": y, "dx": dx, "dy": dy},
        )

    def recipe(self, request: Mapping[str, Any]) -> Any:
        """Run many CUA steps in one request. The guest lints the plan first."""
        return self._auth("POST", f"{self.exec_url}/v1/cua/recipe", dict(request))

    def cua_tree(self, display: str | None = None, **fields: Any) -> Any:
        """The screen's accessibility tree (AT-SPI): buttons, fields and tabs as text.

        ``fields`` are ``scope`` (``"focused_window"``, the default, or ``"desktop"``),
        ``max_depth``, ``max_nodes``, ``timeout_ms``, ``include_hidden`` and ``compact``.
        Each node's ``ref`` is for ``cua_element``, on this tree only; ``bounds`` are screen
        pixels, as ``click`` takes them. ``display`` is another of the box's screens (``:N``).
        """
        url = self._on_screen(f"{self.exec_url}/v1/cua/tree", display)
        return self._auth("POST", url, dict(fields))

    def cua_element(
        self,
        ref: str,
        action: str,
        text: str | None = None,
        display: str | None = None,
    ) -> Any:
        """``press``, ``click``, ``focus`` or ``set_text`` (with ``text``) on a node of the
        screen's latest tree. A ref from an older tree fails with 409 ``stale_ref``."""
        body: dict[str, Any] = {"ref": ref, "action": action}
        if text is not None:
            body["text"] = text
        url = self._on_screen(f"{self.exec_url}/v1/cua/element", display)
        return self._auth("POST", url, body)

    def browser_snap(self, display: str | None = None, **fields: Any) -> Any:
        """The current tab's controls, each with an ``@eN`` ref good for this snapshot alone.

        ``fields``: ``max_nodes`` (1 to 1000, default 200). A password field's text never comes.
        """
        return self._auth("POST", self._on_screen(f"{self.exec_url}/v1/browser/snap", display), dict(fields))

    def browser_act(
        self,
        ref: str,
        action: str,
        text: str | None = None,
        confirm: str | None = None,
        snapshot: int | None = None,
        allow_domains: list[str] | None = None,
        display: str | None = None,
    ) -> Any:
        """``click``, ``type``, ``select``, ``key``, ``scroll`` or ``focus`` on an element by ref,
        through ultra-instinct's ticketed guard. A click on a label like ``Pay*`` fails with 409
        ``confirm_required``; its ``error.details.confirm`` goes back as ``confirm`` once a person
        approved ``error.details.label``."""
        body: dict[str, Any] = {"ref": ref, "action": action}
        for key, value in (
            ("text", text),
            ("confirm", confirm),
            ("snapshot", snapshot),
            ("allow_domains", allow_domains),
        ):
            if value is not None:
                body[key] = value
        return self._auth("POST", self._on_screen(f"{self.exec_url}/v1/browser/act", display), body)

    def browser_navigate(
        self, url: str, allow_domains: list[str] | None = None, display: str | None = None
    ) -> Any:
        body: dict[str, Any] = {"url": url}
        if allow_domains is not None:
            body["allow_domains"] = allow_domains
        return self._auth("POST", self._on_screen(f"{self.exec_url}/v1/browser/navigate", display), body)

    def browser_tabs(self, op: str, display: str | None = None, **fields: Any) -> Any:
        """``op`` is ``list``, ``open`` (``url``), ``close`` or ``switch`` (``tab_id``)."""
        body = {"op": op, **fields}
        return self._auth("POST", self._on_screen(f"{self.exec_url}/v1/browser/tabs", display), body)

    def browser_wait(self, display: str | None = None, **fields: Any) -> Any:
        """Until ``text``, ``url``, ``control``, ``gone`` or ``load`` holds, or ``timeout_ms``
        (default 10000, at most 60000) runs out."""
        wait = float(fields.get("timeout_ms", 10_000)) / 1000
        url = self._on_screen(f"{self.exec_url}/v1/browser/wait", display)
        return self._auth("POST", url, dict(fields), timeout=wait + BROWSER_MARGIN)

    def browser_shot(self, display: str | None = None) -> Any:
        """The current tab's viewport: ``{format, data, bytes, width, height}``, ``data`` base64."""
        return self._auth("POST", self._on_screen(f"{self.exec_url}/v1/browser/shot", display), {})

    def browser_run(self, goal: str, display: str | None = None, **fields: Any) -> Any:
        """Instinct runs ``goal`` until it's done, blocked or abstains: ``max_steps``,
        ``timeout_ms`` (default 120000), ``allow_domains``, ``confirm``."""
        run = float(fields.get("timeout_ms", 120_000)) / 1000
        url = self._on_screen(f"{self.exec_url}/v1/browser/run", display)
        return self._auth("POST", url, {"goal": goal, **fields}, timeout=run + BROWSER_MARGIN)

    def browser_state(self, op: str, name: str | None = None, display: str | None = None) -> Any:
        """``save`` or ``load`` this screen's login, a 0600 file only box-exec can read."""
        body: dict[str, Any] = {"op": op}
        if name is not None:
            body["name"] = name
        return self._auth("POST", self._on_screen(f"{self.exec_url}/v1/browser/state", display), body)

    @staticmethod
    def _on_screen(url: str, display: str | None) -> str:
        return f"{url}?{urlencode({'display': display})}" if display else url

    def _public(self, url: str) -> Any:
        response = self._http.get(url)
        return self._decode(response, url)

    def _auth(
        self, method: str, url: str, json: Any | None = None, timeout: float | None = None
    ) -> Any:
        headers = {
            "authorization": f"Bearer {self.token}",
            "x-request-id": self._request_id(),
        }
        extra: dict[str, Any] = {}
        if timeout is not None and timeout > self._timeout:
            extra["timeout"] = timeout
        response = self._http.request(method, url, headers=headers, json=json, **extra)
        return self._decode(response, url)

    def _body(self, response: httpx.Response) -> Any:
        try:
            return response.json()
        except Exception:
            return {"raw": response.text}

    def _request_id(self) -> str:
        import uuid

        return str(uuid.uuid4())

    def _decode(self, response: httpx.Response, url: str) -> Any:
        body: Any
        try:
            body = response.json()
        except Exception:
            body = {"raw": response.text}
        if response.status_code >= 400:
            raise BuwizComputerError(f"{url} returned {response.status_code}", response.status_code, body)
        return body
