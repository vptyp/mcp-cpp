#!/usr/bin/env python3
"""Command line interface for the C++ MCP server.

Output is YAML by default: one fact per line, no decoration, no ANSI. Data goes
to stdout, errors go to stderr with a non-zero exit code, so the tool composes
in pipelines and is unambiguous to read.
"""

import argparse
import errno
import json
import os
import re
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request
from typing import Any, Dict, List, Optional
from uuid import uuid4

# Per-project cache file. Stores connection vars (transport, http_url,
# session_id, server_path) so repeat calls auto-connect without re-passing flags.
DEFAULT_CONFIG_FILE = ".lsp-cli.json"

class McpCliError(Exception):
    """Custom exception for MCP CLI errors"""
    pass


class _StaleSessionError(McpCliError):
    """Internal: the cached HTTP session is no longer valid on the server."""
    pass


class McpClient:
    """JSON-RPC client for communicating with the MCP server"""
    
    def __init__(
        self,
        server_path: Optional[str] = None,
        fifo_path: Optional[str] = None,
        output_path: Optional[str] = None,
        attach_timeout: float = 30.0,
        http_url: Optional[str] = None,
        session_id: Optional[str] = None,
    ):
        self.server_path = server_path
        self.fifo_path = fifo_path
        self.output_path = output_path
        self.attach_timeout = attach_timeout
        self.http_url = http_url
        self.session_id = session_id
        
    def _validate_server(self) -> None:
        """Validate that the MCP server exists and is executable"""
        if not os.path.exists(self.server_path):
            raise McpCliError(f"MCP server not found at: {self.server_path}")
        if not os.access(self.server_path, os.X_OK):
            raise McpCliError(f"MCP server is not executable: {self.server_path}")
    
    def _send_request(self, method: str, params: Optional[Dict] = None) -> Dict:
        """Send a JSON-RPC request to the MCP server and return the response"""
        if self.http_url:
            return self._send_request_http(method, params)
        if self.fifo_path:
            return self._send_request_attached(method, params)
        return self._send_request_spawned(method, params)

    def _http_exchange(self, payload: Dict) -> Dict:
        """POST a JSON-RPC payload to the streamable-http endpoint and parse the response.

        Handles both plain JSON responses (application/json) and SSE
        (text/event-stream), capturing the ``Mcp-Session-Id`` header on responses
        that (re)establish a session.
        """
        headers = {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
        }
        if self.session_id:
            headers["Mcp-Session-Id"] = self.session_id
        request = urllib.request.Request(
            self.http_url,
            data=json.dumps(payload).encode("utf-8"),
            headers=headers,
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=self.attach_timeout) as resp:
                content_type = resp.headers.get("Content-Type", "")
                new_session_id = resp.headers.get("Mcp-Session-Id")
                body = resp.read()
        except urllib.error.HTTPError as e:
            # A 404 on the endpoint with a session id means the cached session
            # is stale (e.g. the server was restarted). Signal a retry.
            if e.code == 404 and self.session_id:
                self.session_id = None
                raise _StaleSessionError("cached session no longer valid")
            raise McpCliError(f"HTTP request to {self.http_url} failed: {e}")
        except urllib.error.URLError as e:
            raise McpCliError(f"HTTP request to {self.http_url} failed: {e}")
        except OSError as e:
            raise McpCliError(f"Could not connect to {self.http_url}: {e}")

        if new_session_id:
            self.session_id = new_session_id
        text = body.decode("utf-8", errors="replace")

        if "text/event-stream" in content_type:
            data_line = None
            for line in text.splitlines():
                line = line.strip()
                if line.startswith("data:"):
                    d = line[5:].strip()
                    if d and d != "[DONE]":
                        data_line = d
            if data_line is None:
                raise McpCliError("No SSE data received from MCP server")
            return json.loads(data_line)
        return json.loads(text)

    def _initialize_http(self) -> None:
        """Send the MCP `initialize` request to establish an HTTP session."""
        init = {
            "jsonrpc": "2.0",
            "id": str(uuid4()),
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "lsp-cli", "version": "1.0"},
            },
        }
        self._http_exchange(init)

    def _send_request_http(self, method: str, params: Optional[Dict] = None) -> Dict:
        """Send a JSON-RPC request to a running streamable-http MCP server.

        Initializes a session if needed (reusing a cached ``session_id`` when
        available) and retries once if the session has gone stale.
        """
        request = {"jsonrpc": "2.0", "id": str(uuid4()), "method": method}
        if params:
            request["params"] = params

        try:
            if not self.session_id:
                self._initialize_http()
            payload = self._http_exchange(request)
        except _StaleSessionError:
            # Cached session expired server-side; start a fresh one and retry.
            self.session_id = None
            self._initialize_http()
            payload = self._http_exchange(request)

        if "error" in payload:
            error = payload["error"]
            # Some servers report a stale session as a JSON-RPC error instead of
            # an HTTP 404; retry once with a fresh session.
            if error.get("code") == -32015:
                self.session_id = None
                self._initialize_http()
                payload = self._http_exchange(request)
            else:
                raise McpCliError(
                    f"Server error ({error.get('code', 'unknown')}): {error.get('message', 'Unknown error')}"
                )
        return payload

    def _send_request_attached(self, method: str, params: Optional[Dict] = None) -> Dict:
        """Send a JSON-RPC request to an already-running MCP server via a FIFO.

        The running server reads requests from ``fifo_path`` (its stdin) and writes
        responses to ``output_path`` (its stdout). Responses are correlated to the
        request by the JSON-RPC ``id`` field.
        """
        request = {
            "jsonrpc": "2.0",
            "id": str(uuid4()),
            "method": method,
        }
        if params:
            request["params"] = params
        request_id = request["id"]

        # Write the request to the FIFO. Use O_NONBLOCK so we fail fast (ENXIO)
        # if no server is currently reading from the FIFO.
        try:
            fd = os.open(self.fifo_path, os.O_WRONLY | os.O_NONBLOCK)
        except OSError as e:
            if e.errno == errno.ENXIO:
                raise McpCliError(
                    f"No reader on FIFO '{self.fifo_path}'. Is the MCP server running and attached to this FIFO?"
                )
            raise McpCliError(f"Could not open FIFO '{self.fifo_path}': {e}")

        try:
            os.write(fd, (json.dumps(request) + "\n").encode())
        except OSError as e:
            raise McpCliError(f"Could not write to FIFO '{self.fifo_path}': {e}")
        finally:
            os.close(fd)

        # Poll the output log for a response with the matching id.
        deadline = time.time() + self.attach_timeout
        last_size = 0
        while time.time() < deadline:
            try:
                with open(self.output_path, "r", errors="replace") as f:
                    f.seek(last_size)
                    new_content = f.read()
                    last_size = f.tell()
            except FileNotFoundError:
                new_content = ""

            for line in new_content.splitlines():
                line = line.strip()
                if not line:
                    continue
                try:
                    resp = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if resp.get("id") == request_id:
                    if "error" in resp:
                        error = resp["error"]
                        raise McpCliError(
                            f"Server error ({error.get('code', 'unknown')}): {error.get('message', 'Unknown error')}"
                        )
                    return resp

            time.sleep(0.2)

        raise McpCliError(
            f"Timed out after {self.attach_timeout}s waiting for response from attached server"
        )
    
    def _send_request_spawned(self, method: str, params: Optional[Dict] = None) -> Dict:
        """Send a JSON-RPC request by spawning a fresh MCP server process"""
        self._validate_server()
        
        request = {
            "jsonrpc": "2.0",
            "id": str(uuid4()),
            "method": method
        }
        
        if params:
            request["params"] = params
            
        try:
            # Start the MCP server process
            process = subprocess.Popen(
                [self.server_path],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,  # Discard stderr as requested
                text=True
            )
            
            # Send the request
            request_json = json.dumps(request)
            stdout, _ = process.communicate(input=request_json)
            
            if process.returncode != 0:
                raise McpCliError(f"MCP server exited with code {process.returncode}")
            
            # Parse the response
            try:
                response = json.loads(stdout.strip())
            except json.JSONDecodeError as e:
                raise McpCliError(f"Invalid JSON response from server: {e}")
                
            # Check for JSON-RPC errors
            if "error" in response:
                error = response["error"]
                raise McpCliError(f"Server error ({error.get('code', 'unknown')}): {error.get('message', 'Unknown error')}")
                
            return response
            
        except subprocess.TimeoutExpired:
            raise McpCliError("MCP server timed out")
        except FileNotFoundError:
            raise McpCliError(f"Could not execute MCP server: {self.server_path}")
    
    def call_tool(self, name: str, arguments: Dict) -> Dict:
        """Call a specific tool with arguments"""
        params = {
            "name": name,
            "arguments": arguments
        }
        return self._send_request("tools/call", params)




# ---------------------------------------------------------------------------
# Server and config discovery
# ---------------------------------------------------------------------------


def find_server_binary() -> str:
    """Locate the mcp-cpp-server binary.

    Checks PATH first, then walks up from the current directory looking for a
    cargo build output, so the CLI works inside a checkout without installing.
    """
    found = shutil.which("mcp-cpp-server")
    if found:
        return found

    d = os.path.abspath(os.getcwd())
    while True:
        for profile in ("release", "debug"):
            candidate = os.path.join(d, "target", profile, "mcp-cpp-server")
            if os.access(candidate, os.X_OK):
                return candidate
        parent = os.path.dirname(d)
        if parent == d:
            break
        d = parent

    raise McpCliError(
        "Could not find mcp-cpp-server. Build it with 'cargo build --release', "
        "install it in PATH, or pass --server-path."
    )


def _find_config_file(explicit: Optional[str] = None) -> Optional[str]:
    """Locate the .lsp-cli.json cache file by walking up from the current directory."""
    if explicit:
        return explicit
    d = os.path.abspath(os.getcwd())
    while True:
        candidate = os.path.join(d, DEFAULT_CONFIG_FILE)
        if os.path.exists(candidate):
            return candidate
        parent = os.path.dirname(d)
        if parent == d:
            return None
        d = parent


def _load_config(config_path: Optional[str] = None):
    """Load the cache file, returning ``(cache, resolved_path_or_None)``."""
    path = _find_config_file(config_path)
    if not path:
        return {}, None
    try:
        with open(path, "r") as f:
            return json.load(f), path
    except (OSError, json.JSONDecodeError):
        return {}, path


def _save_config(cache: Dict, path: str) -> None:
    """Write the cache file. Failures are warnings, not errors."""
    if not path:
        return
    try:
        with open(path, "w") as f:
            json.dump(cache, f, indent=2)
    except OSError as e:
        print(f"warning: could not write {path}: {e}", file=sys.stderr)


def _project_root(config_path: Optional[str]) -> str:
    """Directory that output paths are reported relative to.

    The directory holding .lsp-cli.json when one was found, otherwise the
    current directory. Keeps paths stable no matter which subdirectory the
    CLI is invoked from.
    """
    if config_path:
        return os.path.dirname(os.path.abspath(config_path))
    return os.path.abspath(os.getcwd())


# ---------------------------------------------------------------------------
# YAML output
# ---------------------------------------------------------------------------

# Scalars starting with these characters, or matching a YAML keyword or number,
# must be quoted to round-trip. Everything else is emitted bare.
_YAML_INDICATORS = "-?:,[]{}#&*!|>'\"%@`"
# Bare words YAML resolves to something other than a string. "=" is the
# rarely-seen YAML "value" tag, which errors out rather than round-tripping.
_YAML_KEYWORDS = {"true", "false", "null", "yes", "no", "on", "off", "~", "=", "<<", ""}

_LOCATION_RE = re.compile(r"^(.*?)(:\d+:\d+(?:-\d+(?::\d+)?)?)$")


def _needs_quotes(s: str) -> bool:
    if s.strip() != s or s.lower() in _YAML_KEYWORDS:
        return True
    # Control characters (tabs especially) cannot appear in a plain scalar.
    if any(c < " " or c == "\x7f" for c in s):
        return True
    if s[0] in _YAML_INDICATORS:
        return True
    # ':' only ends a key when followed by a space or the end of the line, and
    # '#' only starts a comment after a space. Anything else is a plain scalar,
    # which keeps paths like src/a.cpp:10:5 unquoted.
    if ": " in s or s.endswith(":") or " #" in s:
        return True
    # YAML resolves more number forms than float() does -- hex (0x10), octal,
    # underscore-separated (6_), sexagesimal (1:30). Quote anything that opens
    # like a number and has no space, rather than enumerate every form.
    if s[0] in "+-.0123456789" and not any(c.isspace() for c in s):
        return True
    return False


def _scalar(value: Any) -> str:
    """Render a Python scalar as a YAML scalar."""
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return str(value)
    s = str(value)
    # json.dumps produces a double-quoted form YAML accepts verbatim.
    return json.dumps(s) if _needs_quotes(s) else s


def _block_scalar(s: str, indent: int) -> Optional[List[str]]:
    """Render a multi-line string as a YAML literal block, or None if unsafe.

    Documentation and hover text arrive with embedded newlines. A block scalar
    keeps them readable instead of collapsing them into one escaped line.
    Strings a block cannot represent faithfully (trailing spaces, other control
    characters, a leading blank line) fall back to the quoted form.
    """
    if "\n" not in s:
        return None
    lines = s.split("\n")
    if any(line != line.rstrip() for line in lines):
        return None
    if any(c < " " and c != "\n" for c in s):
        return None
    if lines[0].startswith(" ") or lines[0] == "":
        return None

    # Only the strip form is used. Keeping a trailing newline depends on the
    # document itself ending in one, which is not something this emitter can
    # guarantee, so text ending in a newline falls back to the quoted form.
    if s.endswith("\n"):
        return None
    pad = "  " * (indent + 1)
    return ["|-"] + [(pad + line if line else "") for line in lines]


def _dump_mapping(mapping: Dict, indent: int, lines: List[str]) -> None:
    pad = "  " * indent
    for key, value in mapping.items():
        k = _scalar(key)
        if isinstance(value, dict):
            if value:
                lines.append(f"{pad}{k}:")
                _dump_mapping(value, indent + 1, lines)
            else:
                lines.append(f"{pad}{k}: {{}}")
        elif isinstance(value, list):
            if value:
                lines.append(f"{pad}{k}:")
                _dump_sequence(value, indent, lines)
            else:
                lines.append(f"{pad}{k}: []")
        elif isinstance(value, str) and (block := _block_scalar(value, indent)):
            lines.append(f"{pad}{k}: {block[0]}")
            lines.extend(block[1:])
        else:
            lines.append(f"{pad}{k}: {_scalar(value)}")


def _dump_sequence(seq: List, indent: int, lines: List[str]) -> None:
    pad = "  " * (indent + 1)
    for item in seq:
        if isinstance(item, dict) and item:
            # Render the item one level deeper, then turn its first line into
            # the "- " entry. Continuation lines already sit at the right depth.
            sub: List[str] = []
            _dump_mapping(item, indent + 2, sub)
            lines.append(f"{pad}- {sub[0].lstrip()}")
            lines.extend(sub[1:])
        elif isinstance(item, list) and item:
            sub = []
            _dump_sequence(item, indent + 1, sub)
            lines.append(f"{pad}- {sub[0].lstrip()}")
            lines.extend(sub[1:])
        elif isinstance(item, (dict, list)):
            lines.append(f"{pad}- {'{}' if isinstance(item, dict) else '[]'}")
        else:
            lines.append(f"{pad}- {_scalar(item)}")


def to_yaml(data: Any) -> str:
    """Render a JSON-compatible value as YAML."""
    lines: List[str] = []
    if isinstance(data, dict):
        if not data:
            return "{}"
        _dump_mapping(data, 0, lines)
    elif isinstance(data, list):
        if not data:
            return "[]"
        _dump_sequence(data, -1, lines)
    else:
        lines.append(_scalar(data))
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# Response reshaping
# ---------------------------------------------------------------------------

# LSP SymbolKind numeric values -> readable names. The server emits numbers
# (5 = Class), which are opaque to a reader; translate them at the edge.
_SYMBOL_KINDS = {
    1: "File", 2: "Module", 3: "Namespace", 4: "Package", 5: "Class",
    6: "Method", 7: "Property", 8: "Field", 9: "Constructor", 10: "Enum",
    11: "Interface", 12: "Function", 13: "Variable", 14: "Constant",
    15: "String", 16: "Number", 17: "Boolean", 18: "Array", 19: "Object",
    20: "Key", 21: "Null", 22: "EnumMember", 23: "Struct", 24: "Event",
    25: "Operator", 26: "TypeParameter",
}


def _translate_symbol_kinds(obj: Any) -> Any:
    """Recursively replace numeric LSP SymbolKind values with their names."""
    if isinstance(obj, dict):
        kind = obj.get("kind")
        if isinstance(kind, int) and kind in _SYMBOL_KINDS:
            obj["kind"] = _SYMBOL_KINDS[kind]
        for value in obj.values():
            _translate_symbol_kinds(value)
    elif isinstance(obj, list):
        for item in obj:
            _translate_symbol_kinds(item)
    return obj


def _relativize(path: str, root: str) -> str:
    """Shorten an absolute path that lives under the project root."""
    if not path.startswith("/"):
        return path
    try:
        rel = os.path.relpath(path, root)
    except ValueError:
        return path
    return path if rel.startswith("..") else rel


def _shorten_location(location: Any, root: str) -> Any:
    """Relativize the file part of a 'path:line:col' location string."""
    if not isinstance(location, str):
        return location
    match = _LOCATION_RE.match(location)
    if not match:
        return _relativize(location, root)
    return _relativize(match.group(1), root) + match.group(2)


def _shorten_paths(obj: Any, root: str) -> Any:
    """Relativize every path-like string in the payload, in place."""
    if isinstance(obj, dict):
        for key, value in obj.items():
            if isinstance(value, str):
                obj[key] = _shorten_location(value, root)
            else:
                _shorten_paths(value, root)
    elif isinstance(obj, list):
        for i, item in enumerate(obj):
            if isinstance(item, str):
                obj[i] = _shorten_location(item, root)
            else:
                _shorten_paths(item, root)
    return obj


def _qualified_name(symbol: Dict) -> str:
    """Join container and name into the name a C++ developer would write."""
    name = str(symbol.get("name", ""))
    container = symbol.get("container_name")
    if container and not name.startswith(f"{container}::"):
        return f"{container}::{name}"
    return name


def _symbol_line(symbol: Dict) -> str:
    """Collapse a symbol to one unambiguous line: name | kind | location."""
    return " | ".join(
        (
            _qualified_name(symbol),
            str(symbol.get("kind", "Unknown")),
            str(symbol.get("location", "")),
        )
    )


def _collapse_symbol_lists(obj: Any) -> Any:
    """Replace lists of symbol objects with one-line-per-symbol strings.

    A symbol carries name, kind and location and nothing else worth a nested
    block, so a flat line is both shorter and easier to scan.
    """
    if isinstance(obj, dict):
        for key, value in obj.items():
            obj[key] = _collapse_symbol_lists(value)
        return obj
    if isinstance(obj, list):
        if obj and all(
            isinstance(i, dict) and "name" in i and "kind" in i and "location" in i
            for i in obj
        ):
            return [_symbol_line(i) for i in obj]
        return [_collapse_symbol_lists(i) for i in obj]
    return obj


def _drop_nulls(obj: Any) -> Any:
    """Remove keys whose value is null.

    An absent field and a null field mean the same thing here, and printing the
    null only adds a line the reader has to dismiss.
    """
    if isinstance(obj, dict):
        return {k: _drop_nulls(v) for k, v in obj.items() if v is not None}
    if isinstance(obj, list):
        return [_drop_nulls(i) for i in obj]
    return obj


def _is_tool_error(response: Dict) -> bool:
    """True when the server reported the tool call itself as failed."""
    result = response.get("result")
    return isinstance(result, dict) and bool(result.get("isError"))


def _error_text(response: Dict) -> str:
    """Pull the human-readable message out of a failed tool call."""
    result = response.get("result") or {}
    parts = [
        c["text"]
        for c in (result.get("content") or [])
        if isinstance(c, dict) and isinstance(c.get("text"), str)
    ]
    return "\n".join(parts) if parts else "tool call failed"


def extract_payload(response: Dict) -> Any:
    """Pull the tool result out of the JSON-RPC envelope.

    Tool results arrive as a JSON document inside a text content block; unwrap
    it so downstream formatting sees plain data.
    """
    result = response.get("result")
    if not isinstance(result, dict):
        raise McpCliError("Malformed response: missing 'result'")

    content = result.get("content")
    if not content:
        return result

    texts = [c["text"] for c in content if isinstance(c, dict) and "text" in c]
    if not texts:
        return result

    joined = "\n".join(texts)
    try:
        return json.loads(joined)
    except json.JSONDecodeError:
        return joined


def shape_payload(payload: Any, root: str) -> Any:
    """Apply the output contract: readable kinds, short paths, flat symbols.

    'success' is dropped because failure is already reported by a non-zero exit
    code, so it never carries information on the path where it is printed.
    """
    if isinstance(payload, dict):
        payload.pop("success", None)
    _translate_symbol_kinds(payload)
    _shorten_paths(payload, root)
    return _drop_nulls(_collapse_symbol_lists(payload))


# ---------------------------------------------------------------------------
# Command line interface
# ---------------------------------------------------------------------------

SYMBOL_KIND_CHOICES = sorted(set(_SYMBOL_KINDS.values()))

_EPILOG = """\
examples:
  lsp-cli.py project                             list build directories to use below
  lsp-cli.py index --build-directory build       check whether clangd has finished indexing
  lsp-cli.py search Math                         find symbols whose name matches "Math"
  lsp-cli.py search Buffer --kind Class Struct   restrict the search to types
  lsp-cli.py search "" --files src/main.cpp      list every symbol defined in one file
  lsp-cli.py analyze Math::factorial             definition, callers, members and usages
  lsp-cli.py diagnostics src/main.cpp src/util.cpp  compiler errors and warnings for one or more files

output:
  YAML on stdout. Errors go to stderr and exit non-zero.
  Symbols are printed one per line as: name | kind | path:line:column
  Paths are relative to the project root; lines and columns are 1-based.
"""


def _add_build_directory(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--build-directory",
        metavar="DIR",
        help="Build directory holding compile_commands.json. "
             "Defaults to the one the server auto-detects; run 'project' to list them.",
    )


def _add_wait_timeout(parser: argparse.ArgumentParser, what: str) -> None:
    parser.add_argument(
        "--wait-timeout",
        type=int,
        metavar="SECONDS",
        help=f"Seconds to wait for {what} before answering. 0 answers immediately.",
    )


def create_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="lsp-cli.py",
        description="Query a C++ codebase semantically through clangd.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=_EPILOG,
    )
    parser.add_argument(
        "--format",
        choices=("yaml", "json", "raw"),
        default="yaml",
        help="yaml: readable output (default). "
             "json: the tool result as JSON. "
             "raw: the full JSON-RPC response, unmodified.",
    )
    parser.add_argument(
        "--server-path",
        metavar="PATH",
        help="mcp-cpp-server binary to run. Found on PATH or under ./target by default.",
    )
    parser.add_argument(
        "--http-url",
        metavar="URL",
        help="Talk to a running server over HTTP instead of spawning one, "
             "e.g. http://127.0.0.1:8080/mcp",
    )
    parser.add_argument(
        "--config",
        metavar="PATH",
        help=f"Connection cache file (default: nearest {DEFAULT_CONFIG_FILE} in a parent directory).",
    )
    parser.add_argument(
        "--debug",
        action="store_true",
        help="Print a full traceback when the CLI itself fails.",
    )

    # Attach transport: kept working for the E2E harness, hidden from --help
    # because it is test plumbing rather than a user-facing feature.
    parser.add_argument("--attach", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--fifo", help=argparse.SUPPRESS)
    parser.add_argument("--output", help=argparse.SUPPRESS)
    parser.add_argument("--attach-timeout", type=float, default=30.0, help=argparse.SUPPRESS)

    commands = parser.add_subparsers(dest="command", metavar="<command>")

    project = commands.add_parser(
        "project",
        aliases=["get-project-details"],
        help="Show the project layout: build directories and compilation databases",
    )
    project.add_argument("--path", metavar="DIR", help="Project root to scan instead of the server's.")
    project.add_argument(
        "--depth", type=int, choices=range(0, 11), metavar="0-10",
        help="How many directory levels to search for build directories.",
    )
    project.add_argument(
        "--include-details", action="store_true",
        help="Also list every build option and configuration variable. Verbose.",
    )

    index = commands.add_parser(
        "index",
        aliases=["get-index-status"],
        help="Show how far clangd has got indexing a build directory",
    )
    _add_build_directory(index)
    _add_wait_timeout(index, "indexing to finish")

    search = commands.add_parser(
        "search",
        aliases=["search-symbols"],
        help="Find symbols by name across the project",
    )
    search.add_argument(
        "query", nargs="?", default="",
        help="Name to match. Fuzzy and qualified names both work "
             "(Math, Math::factorial). Pass \"\" with --files to list a file's symbols.",
    )
    search.add_argument(
        "--kind", nargs="+", dest="kinds", metavar="KIND", choices=SYMBOL_KIND_CHOICES,
        help="Keep only these kinds. One or more of: " + ", ".join(SYMBOL_KIND_CHOICES),
    )
    search.add_argument(
        "--files", nargs="+", metavar="FILE",
        help="Search inside these files only. Slower, but returns every match; "
             "without it clangd's index may omit some.",
    )
    search.add_argument(
        "--max-results", type=int, default=100, metavar="N",
        help="Stop after N symbols (default: 100).",
    )
    search.add_argument(
        "--include-external", action="store_true",
        help="Also match symbols from system headers and third-party libraries.",
    )
    _add_build_directory(search)
    _add_wait_timeout(search, "indexing to finish")

    analyze = commands.add_parser(
        "analyze",
        aliases=["analyze-symbol"],
        help="Show a symbol's definition, callers, members and example usages",
    )
    analyze.add_argument("symbol", help="Symbol to analyze, e.g. Math::factorial or MyClass.")
    analyze.add_argument(
        "--max-examples", type=int, metavar="N",
        help="Cap the number of usage examples. All of them by default.",
    )
    analyze.add_argument(
        "--location-hint", metavar="FILE:LINE:COL",
        help="Pick a specific overload by where it is declared. Lines and columns are 1-based.",
    )
    _add_build_directory(analyze)
    _add_wait_timeout(analyze, "indexing to finish")

    diagnostics = commands.add_parser(
        "diagnostics",
        aliases=["show-diagnostics"],
        help="Show clangd errors, warnings and notes for one or more source files",
    )
    diagnostics.add_argument(
        "file", nargs="+",
        help="Source file(s) to check. Relative paths resolve against the project root.",
    )
    _add_build_directory(diagnostics)
    _add_wait_timeout(diagnostics, "clangd to report")

    return parser


# Subcommand name (including aliases) -> MCP tool name.
_TOOL_FOR_COMMAND = {
    "project": "get_project_details",
    "get-project-details": "get_project_details",
    "index": "get_index_status",
    "get-index-status": "get_index_status",
    "search": "search_symbols",
    "search-symbols": "search_symbols",
    "analyze": "analyze_symbol_context",
    "analyze-symbol": "analyze_symbol_context",
    "diagnostics": "show_diagnostics",
    "show-diagnostics": "show_diagnostics",
}

# Subcommand -> (CLI attribute, tool argument). Attributes left at their
# default are omitted so the server applies its own defaults.
_ARGUMENTS_FOR_TOOL = {
    "get_project_details": [("path", "path"), ("depth", "depth"), ("include_details", "include_details")],
    "get_index_status": [("build_directory", "build_directory"), ("wait_timeout", "wait_timeout")],
    "search_symbols": [
        ("query", "query"), ("kinds", "kinds"), ("files", "files"),
        ("max_results", "max_results"), ("include_external", "include_external"),
        ("build_directory", "build_directory"), ("wait_timeout", "wait_timeout"),
    ],
    "analyze_symbol_context": [
        ("symbol", "symbol"), ("max_examples", "max_examples"),
        ("location_hint", "location_hint"), ("build_directory", "build_directory"),
        ("wait_timeout", "wait_timeout"),
    ],
    "show_diagnostics": [
        ("file", "file"), ("build_directory", "build_directory"), ("wait_timeout", "wait_timeout"),
    ],
}


def build_tool_arguments(tool: str, args: argparse.Namespace) -> Dict:
    """Turn parsed CLI arguments into the MCP tool's argument object."""
    arguments: Dict[str, Any] = {}
    for attribute, name in _ARGUMENTS_FOR_TOOL[tool]:
        value = getattr(args, attribute, None)
        # None means "not given"; False and [] mean "flag absent". An empty
        # query string is meaningful, so it is passed through explicitly.
        if value is None or value == [] or value is False:
            continue
        arguments[name] = value
    if tool == "search_symbols":
        arguments["query"] = args.query
    return arguments


def _with_file(args: argparse.Namespace, file: str) -> argparse.Namespace:
    """Shallow copy of args with the diagnostics file replaced."""
    copy = argparse.Namespace(**vars(args))
    copy.file = file
    return copy


def create_client(args: argparse.Namespace, config: Dict) -> "McpClient":
    """Pick a transport: explicit HTTP, cached HTTP, attached FIFO, or spawn."""
    http_url = args.http_url or config.get("http_url")
    if http_url:
        return McpClient(
            http_url=http_url,
            session_id=config.get("session_id"),
            attach_timeout=args.attach_timeout,
        )
    if args.attach:
        if not args.fifo or not args.output:
            raise McpCliError("--attach requires both --fifo and --output")
        return McpClient(
            fifo_path=args.fifo,
            output_path=args.output,
            attach_timeout=args.attach_timeout,
        )
    return McpClient(args.server_path or find_server_binary())


def run(args: argparse.Namespace) -> int:
    config, config_path = _load_config(args.config)
    client = create_client(args, config)

    tool = _TOOL_FOR_COMMAND[args.command]

    # Diagnostics accepts one or more files: call the tool once per file and
    # aggregate, so one invocation covers a whole change set. _with_file sets
    # a single string (the server expects a string, not a list).
    if args.command in ("diagnostics", "show-diagnostics"):
        responses = [
            client.call_tool(tool, build_tool_arguments(tool, _with_file(args, f)))
            for f in args.file
        ]
    else:
        responses = [client.call_tool(tool, build_tool_arguments(tool, args))]

    # Cache the HTTP session next to the project root, not the current
    # directory, so running from a subdirectory does not scatter config files.
    if client.http_url and client.session_id:
        target = args.config or config_path or os.path.join(
            _project_root(config_path), DEFAULT_CONFIG_FILE
        )
        _save_config(
            {
                "transport": "http",
                "http_url": client.http_url,
                "session_id": client.session_id,
                "server_path": args.server_path or config.get("server_path"),
            },
            target,
        )

    if args.format == "raw":
        print(json.dumps(responses if len(responses) > 1 else responses[0], indent=2))
        return 0 if not any(_is_tool_error(r) for r in responses) else 1

    # A tool that fails reports it in the result rather than as a JSON-RPC
    # error, so check for it explicitly; otherwise the message would be
    # printed as if it were data and the exit code would claim success.
    failed = [r for r in responses if _is_tool_error(r)]
    if failed:
        raise McpCliError(_error_text(failed[0]))

    payloads = [extract_payload(r) for r in responses]
    if args.format == "json":
        print(json.dumps(payloads if len(payloads) > 1 else payloads[0], indent=2))
        return 0

    shaped = [shape_payload(p, _project_root(config_path)) for p in payloads]
    if len(shaped) > 1:
        print(to_yaml(shaped))
    else:
        print(to_yaml(shaped[0]))
    return 0


def main() -> int:
    parser = create_parser()
    args = parser.parse_args()

    if not args.command:
        parser.print_help()
        return 2

    try:
        return run(args)
    except McpCliError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("cancelled", file=sys.stderr)
        return 130
    except BrokenPipeError:
        # Downstream closed the pipe (e.g. '| head'); that is not a failure.
        return 0
    except Exception as e:
        # A bug in the CLI, not a server-reported problem. --debug shows where.
        if args.debug:
            raise
        print(f"internal error: {type(e).__name__}: {e}", file=sys.stderr)
        print("re-run with --debug for a traceback", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
