# cmux backend

Status: **initial identity and control slice implemented.** Muxa can identify
the current cmux surface from hook environment, keep its socket endpoint, focus
that exact surface, and send targeted text through cmux's documented Unix
socket API. Full workspace/surface enumeration and sidebar presentation remain
tracked in [issue #90](https://github.com/Open330/muxa/issues/90).

## Identity

cmux exports `CMUX_WORKSPACE_ID` and `CMUX_SURFACE_ID` in managed terminals.
Muxa stores a surface as `cmux:<surface-id>` so it cannot collide with tmux,
rmux, zellij, or herdr pane ids. The hook adapter retains the workspace UUID
on the execution surface and `CMUX_SOCKET_PATH` (falling back to
`/tmp/cmux.sock`) in the existing endpoint field used by control routing. That
metadata is enough to reconstruct a hook-authoritative workspace/surface row
even when muxad started outside cmux and inherited none of its environment.

The logical Workspace → Work → Run → Agent hierarchy remains muxa-owned. A
cmux workspace or surface is an execution binding, not durable Work identity.

## First-slice capabilities

| Capability | Current behavior |
| --- | --- |
| Hook identity | `CMUX_SURFACE_ID` → `cmux:<surface-id>` |
| Current surface | Environment- or hook-backed row with cmux workspace identity |
| Full inventory | Not yet implemented; observations are structurally partial |
| Current command / PID | Unsupported |
| Screen capture | Unsupported by the documented API used in this slice |
| Focus | `surface.focus` over the cmux Unix socket |
| Targeted input | `surface.send_text` over the cmux Unix socket |

Partial observation is intentional: seeing only the invoking surface must
never make the reconciler reap or age out hook-authoritative agents on other
cmux surfaces. Muxa distinguishes this structural `Partial` result from a
normally-authoritative backend's transient `Incomplete` failure. The backend
stays in the daemon's default multi-host set even when
muxad started before cmux, allowing later `cmux:` hook rows to route control
without restarting muxad.

## Socket access

cmux defaults to allowing only processes spawned inside cmux terminals to use
its socket. Operators should keep that mode on shared machines. A separately
launched muxad may observe hook state while focus/input calls are refused by
cmux until its access mode permits that daemon process. `muxa mcp` therefore
uses the recorded endpoint directly when the MCP process is running inside
cmux, preserving the default descendant-only access mode; daemon-hosted web or
fleet control still requires an access mode that admits muxad.

Official API references:

- <https://cmux.com/docs/api>
- <https://cmux.com/docs/dock>

## Next slices

- Lock a supported cmux JSON schema into fixtures and enumerate every
  workspace/surface defensively.
- Add host-neutral managed launch/reuse on top of the execution seam from
  [issue #89](https://github.com/Open330/muxa/issues/89).
- Mirror agent attention/progress into namespaced cmux sidebar status and ship
  an optional Dock control example.
