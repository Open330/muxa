#!/usr/bin/env python3

import argparse
import base64
import json
import os
import socket
import sys
from pathlib import Path

MAXIMUM_RESPONSE_BYTES = 48 * 1024 * 1024


def request(payload: dict) -> dict:
    socket_path = f"/tmp/muxa-qa-helper-{os.getuid()}.sock"
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(15)
        client.connect(socket_path)
        client.sendall(json.dumps(payload, separators=(",", ":")).encode() + b"\n")
        response = bytearray()
        while b"\n" not in response:
            chunk = client.recv(1024 * 1024)
            if not chunk:
                break
            response.extend(chunk)
            if len(response) > MAXIMUM_RESPONSE_BYTES:
                raise RuntimeError("helper response exceeds 48 MiB")
    if b"\n" not in response:
        raise RuntimeError("helper closed without a complete response")
    result = json.loads(bytes(response).split(b"\n", 1)[0])
    if not result.get("ok"):
        raise RuntimeError(result.get("error", "helper request failed"))
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description="Control the owner-only Muxa QA Helper")
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("status")
    commands.add_parser("prompt-permissions")
    commands.add_parser("inspect")
    commands.add_parser("new-shell")

    click = commands.add_parser("click")
    click.add_argument("--x", required=True, type=float, help="x coordinate inside the Muxa window")
    click.add_argument("--y", required=True, type=float, help="y coordinate inside the Muxa window")

    scroll = commands.add_parser("scroll")
    scroll.add_argument("--x", required=True, type=float, help="x coordinate inside the Muxa window")
    scroll.add_argument("--y", required=True, type=float, help="y coordinate inside the Muxa window")
    scroll.add_argument("--dy", required=True, type=float, help="pixels; negative scrolls content down")

    resize = commands.add_parser("resize")
    resize.add_argument("--width", required=True, type=float, help="window width in points")
    resize.add_argument("--height", required=True, type=float, help="window height in points")
    resize.add_argument("--x", type=float, help="optional new window x origin")
    resize.add_argument("--y", type=float, help="optional new window y origin")

    capture = commands.add_parser("capture")
    capture.add_argument("--output", required=True, type=Path)

    type_command = commands.add_parser("type")
    type_command.add_argument("--text", required=True)
    type_command.add_argument("--return", dest="press_return", action="store_true")

    key = commands.add_parser("key", help="press one key, optionally with modifiers")
    key.add_argument(
        "--key",
        required=True,
        help="a single character or one of return/escape/tab/space/up/down/left/right/delete",
    )
    key.add_argument(
        "--mod",
        dest="modifiers",
        action="append",
        default=[],
        choices=["command", "shift", "option", "control"],
        help="modifier to hold while pressing the key; repeat for chords",
    )

    args = parser.parse_args()
    if args.command == "prompt-permissions":
        result = request({"command": "prompt_permissions"})
    elif args.command == "capture":
        result = request({"command": "capture"})
        data = base64.b64decode(result.pop("png_base64"), validate=True)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_bytes(data)
        result["output"] = str(args.output.resolve())
        result["bytes"] = len(data)
    elif args.command == "type":
        result = request(
            {
                "command": "type",
                "text": args.text,
                "press_return": args.press_return,
            }
        )
    elif args.command == "new-shell":
        result = request({"command": "new_shell"})
    elif args.command == "key":
        result = request({"command": "key", "key": args.key, "modifiers": args.modifiers})
    elif args.command == "click":
        result = request({"command": "click", "x": args.x, "y": args.y})
    elif args.command == "scroll":
        result = request({"command": "scroll", "x": args.x, "y": args.y, "delta_y": args.dy})
    elif args.command == "resize":
        payload = {"command": "resize", "width": args.width, "height": args.height}
        if args.x is not None and args.y is not None:
            payload.update({"x": args.x, "y": args.y})
        result = request(payload)
    else:
        result = request({"command": args.command})

    print(json.dumps(result, indent=2, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, ValueError) as error:
        print(f"muxa-qa-helper-client: {error}", file=sys.stderr)
        raise SystemExit(1)
