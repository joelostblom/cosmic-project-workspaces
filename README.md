# COSMIC Project Workspaces

`cpw` is a keyboard-driven prototype for [COSMIC issue #137](https://github.com/pop-os/cosmic-epoch/issues/137). It groups your existing COSMIC workspaces into project columns **without changing COSMIC itself**.

**Left/right** returns to the last workspace you used in the neighboring project, even if you got there via COSMIC's own shortcuts or overview. **Up/down** moves within the current project. There is no wraparound.

## Try it

Run the built binary from this project directory. To build it yourself, run `cargo build --release --locked` with Rust 1.89 or newer; no Wayland development packages are needed.

In one terminal, leave the helper running:

```sh
./target/release/cpw serve
```

On the workspace you want to assign, open a terminal in the project directory and run:

```sh
./target/release/cpw assign Work
```

Repeat on another workspace with a different project name. Assign a third workspace to either project to try vertical navigation:

```sh
./target/release/cpw assign Personal
# On a different workspace:
./target/release/cpw assign Personal --top
./target/release/cpw status
./target/release/cpw left
./target/release/cpw right
```

New projects appear on the right; new rows go at the bottom unless you pass `--top`. `cpw go PROJECT` enters a project by name, including when the active workspace isn't assigned. Unavailable workspaces are skipped; if a project's last-used workspace is unavailable, entry falls back to its top available row. `cpw unassign` removes just the active workspace from the layout. `cpw forget PROJECT` removes a whole column. Neither command closes or unpins any workspaces.

## Banners and animation

When you enter a different project, `cpw` shows its name in a short COSMIC notification, even if you switch with COSMIC's own controls. Moving within a project doesn't show a banner. The notification uses `notify-send` (from `libnotify` on Fedora), stays out of COSMIC's notification history, and respects Do Not Disturb. To disable it, start the helper with `cpw serve --no-notifications`.

COSMIC chooses the slide axis for all switches from its workspace layout setting. `cpw` can't set an axis or direction per switch, so the native slide animation is unchanged.

## Shortcuts and pinning

- **Optional shortcuts:** Add four custom commands under COSMIC Settings → Keyboard → Keyboard Shortcuts. Use the **absolute path** to the built binary, followed by `left`, `right`, `up`, or `down`. Choose unused shortcuts at first so COSMIC's existing navigation remains available. The helper must be running for these commands to work.
- **Keep workspaces:** Pin a workspace in COSMIC's Workspaces overview before or after assigning it. This keeps it from disappearing when empty and lets COSMIC give the helper a stable ID. The helper saves those assignments and their last-used state in `$XDG_CONFIG_HOME/cosmic-project-workspaces/layout.json` (normally under `~/.config`). Unpinned dynamic workspaces are normally session-only: if COSMIC removes one, the helper drops its assignment. The helper does not pin or unpin anything itself.

This prototype requires a single active display. It does not create workspaces, move windows, change touchpad gestures, or replace COSMIC's overview. To stop, press Ctrl+C in the terminal running `serve`.
