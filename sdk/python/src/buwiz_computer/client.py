"""Connect-only HTTP client for a running buwiz-computer guest."""

from __future__ import annotations

from typing import Any, Mapping
from urllib.parse import urlencode

import httpx


class BuwizComputerError(Exception):
    def __init__(self, message: str, status: int, body: Any) -> None:
        super().__init__(message)
        self.status = status
        self.body = body


def _trim_slash(url: str) -> str:
    return url.strip().rstrip("/")


class BuwizComputer:
    def __init__(self, exec_url: str, host_url: str, token: str) -> None:
        self.exec_url = _trim_slash(exec_url)
        self.host_url = _trim_slash(host_url)
        self.token = token
        self._http = httpx.Client(timeout=120.0)

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

    def chrome(self) -> Any:
        return self._auth("GET", f"{self.host_url}/v1/chrome")

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

    def _public(self, url: str) -> Any:
        response = self._http.get(url)
        return self._decode(response, url)

    def _auth(self, method: str, url: str, json: Any | None = None) -> Any:
        headers = {
            "authorization": f"Bearer {self.token}",
            "x-request-id": self._request_id(),
        }
        response = self._http.request(method, url, headers=headers, json=json)
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
