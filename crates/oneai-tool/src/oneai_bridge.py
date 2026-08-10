#!/usr/bin/env python3
"""OneAI code-mode bridge.

Launched (sandboxed) by ``oneai-tool``'s ``CodeInterpreterTool``. Speaks a
line-delimited JSON-RPC with the host over the **real** process stdin/stdout:

  - bridge -> host: one JSON object per line on fd 1 (stdout).
      * request:  ``{"id": <int>, "type": "call", "tool": <name>, "args": {...}}``
      * final:    ``{"type": "done", "stdout": <str>, "stderr": <str>, "error": <str|null>}``
  - host -> bridge: one JSON object per line on fd 0 (stdin).
      * response: ``{"id": <int>, "success": <bool>, "content": <str>, "error": <str|null>}``

The user script's own ``print()`` / stderr are redirected into in-memory
buffers and returned inside the ``done`` message, so they can never corrupt the
RPC channel. The bridge's own RPC I/O bypasses the redirected ``sys.stdout`` by
writing to the raw fd 1 via ``os.write`` and reading from a duplicated fd 0.

Env:
  ONEAI_CODE  — the user's Python source (compiled + exec'd).
  ONEAI_TOOLS — JSON array of ``{"name": str, "description": str}``; each entry
                becomes a keyword-only callable in the user namespace that
                RPCs back to the host tool of the same name.
"""

import contextlib
import io
import json
import os
import sys
import traceback


# A line-buffered text reader over a *duplicate* of fd 0 so that closing it
# (e.g. at interpreter shutdown) does not close the real stdin the host owns.
_STDIN = os.fdopen(os.dup(0), "r", buffering=1)


def _write_line(obj):
    """Write one JSON object + newline to the real fd 1, bypassing any
    redirected ``sys.stdout`` so user ``print()`` cannot corrupt the channel."""
    data = (json.dumps(obj, ensure_ascii=False) + "\n").encode("utf-8")
    os.write(1, data)


_id_counter = [0]


def _next_id():
    _id_counter[0] += 1
    return _id_counter[0]


def _call_tool(name, **args):
    """RPC a single tool call to the host and block on the response."""
    req_id = _next_id()
    _write_line({"id": req_id, "type": "call", "tool": name, "args": args})
    line = _STDIN.readline()
    if not line:
        raise RuntimeError(
            "code mode: host closed the RPC channel before responding to "
            f"tool '{name}'"
        )
    resp = json.loads(line)
    if resp.get("id") != req_id:
        raise RuntimeError(
            f"code mode: RPC id mismatch (expected {req_id}, "
            f"got {resp.get('id')})"
        )
    if not resp.get("success", False):
        raise RuntimeError(
            f"tool '{name}' failed: {resp.get('error', '<no error field>')}"
        )
    return resp.get("content", "")


def _make_tool_fn(name, description):
    def _fn(*args, **kwargs):
        if args:
            raise TypeError(
                f"tool '{name}' must be called with keyword arguments only "
                f"(got {len(args)} positional)"
            )
        return _call_tool(name, **kwargs)

    _fn.__name__ = name
    _fn.__qualname__ = name
    _fn.__doc__ = description
    return _fn


def _build_namespace(tools):
    """User globals: one keyword-only callable per registered tool."""
    ns = {"__name__": "__oneai_code__"}
    for t in tools:
        name = t.get("name")
        if not name:
            continue
        ns[name] = _make_tool_fn(name, t.get("description", ""))
    return ns


def main():
    code = os.environ.get("ONEAI_CODE", "")
    try:
        tools = json.loads(os.environ.get("ONEAI_TOOLS", "[]"))
    except Exception as e:  # noqa: BLE001 — report any env-parse failure to host
        _write_line(
            {
                "type": "done",
                "stdout": "",
                "stderr": "",
                "error": f"code mode: bad ONEAI_TOOLS env: {e}",
            }
        )
        return

    ns = _build_namespace(tools)
    out_buf = io.StringIO()
    err_buf = io.StringIO()
    error = None

    try:
        with contextlib.redirect_stdout(out_buf), contextlib.redirect_stderr(err_buf):
            exec(compile(code, "<oneai_code>", "exec"), ns)
    except SystemExit as e:
        # Explicit sys.exit(): treat as clean termination unless nonzero.
        if e.code not in (None, 0):
            error = f"Script exited with code {e.code!r}"
    except BaseException as e:  # noqa: BLE001 — surface any user-script error
        error = "".join(
            traceback.format_exception(type(e), e, e.__traceback__)
        )

    _write_line(
        {
            "type": "done",
            "stdout": out_buf.getvalue(),
            "stderr": err_buf.getvalue(),
            "error": error,
        }
    )


if __name__ == "__main__":
    main()
