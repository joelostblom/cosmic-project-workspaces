# COSMIC Project Workspaces

`cpw` is a keyboard-driven prototype for [COSMIC issue #137](https://github.com/pop-os/cosmic-epoch/issues/137). It groups your existing COSMIC workspaces into project columns **without modifying COSMIC's source code**.

**Left/right** returns to the last workspace you used in the neighboring project, even if you got there via COSMIC's own shortcuts or overview. **Up/down** moves within the current project. There is no wraparound.

## Try it

Run the built binary from this project directory. To build it yourself, run `cargo build --release --locked` with Rust 1.89 or newer; no Wayland development packages are needed.

In one terminal, leave the helper running:

```sh
./target/release/cpw serve
```

Workspaces without a project are automatically assigned to **Work**, including new workspaces you start using through COSMIC itself. The empty trailing workspace is kept free for any project to borrow. To use a different default, start the helper with `./target/release/cpw serve --default-project "My project"`. This doesn't move any workspaces that already have an assignment.

On a fresh layout, **Work** and **Personal** are ready to use. With a different default, that project comes first, followed by Personal (or Work if Personal is the default). To choose the initial columns and their order, repeat `--project NAME` on the first run. Include the default project in that list:

```sh
./target/release/cpw serve --project Work --project "Side Projects" --project Personal
```

Saved layouts keep their existing columns; `--project` is only used to set up a new one.

### Move workspaces between projects

`cpw move-left` and `cpw move-right` move the **whole active workspace**, with its windows, to the bottom of the adjacent project. It stays active. These commands can move into an empty column; plain `left` and `right` skip empty columns and never create projects. Moving past an outer edge creates a temporary `Project N` on that side. Temporary columns disappear when they lose their last workspace. Use `cpw rename-project NAME` while on a workspace in that column to give it a permanent name. The configured default name is fixed for the current run; change it with `--default-project`.

Named columns stay visible even if they have no workspaces. No extra COSMIC workspaces are created to keep them alive. You can still assign the active workspace by name to create a named column or reach a project that isn't adjacent:

```sh
./target/release/cpw assign Personal
./target/release/cpw status --grid
```

With `assign`, new projects appear on the right; new rows go at the bottom unless you pass `--top`. `cpw go PROJECT` enters a project by name, including from the empty spare. Unavailable workspaces are skipped; if a project's last-used workspace is unavailable, entry falls back to its top available row. `cpw unassign` returns the active workspace to the default project (or frees it if it's the empty trailing workspace). `cpw forget PROJECT` removes a non-default column and returns its available workspaces to the default. Neither command closes or unpins any workspaces.

Press **Down** on a project's bottom row or **Up** on its top row to borrow COSMIC's trailing empty workspace without pinning it. For **Up**, `cpw` moves the spare above the current row and activates it in one transaction so COSMIC can slide upward. For **Down**, the spare stays at the end for the slide; `cpw` syncs it into the project block when COSMIC supplies a fresh spare. Open a window on the new row before growing that edge again. If you leave the row empty, COSMIC can remove it or the helper can return it to the spare pool.

### Keep COSMIC order in sync

Both automatic and manual assignments quietly group assigned workspaces in COSMIC's own list, in project order and then top to bottom. The block starts at the first assigned workspace. The unassigned spare stays at the end. COSMIC's workspace numbers and its native next/previous traversal can change; the active workspace and its windows stay together.

`cpw status --grid` shows the project columns side by side, with the active workspace in brackets. `cpw sync-order --dry-run` previews a reorder without making changes. Run `cpw sync-order` to apply it to an existing layout or repair drift after manually dragging workspaces in COSMIC or starting a new login session. Switching projects doesn't trigger a reorder.

## Banners and animation

When you enter a different project, `cpw` shows its name and row (for example, `Personal (2/3)`) in a short COSMIC notification. This works with COSMIC's own controls and when you transfer the active workspace with `move-left` or `move-right`. Moving within a project doesn't show a banner. The notification uses `notify-send` (from `libnotify` on Fedora), stays out of COSMIC's notification history, and respects Do Not Disturb. To disable it, start the helper with `cpw serve --no-notifications`.

### Experimental slide-axis workaround

The slide workaround is **on by default**. The helper temporarily changes COSMIC's **global** workspace layout to horizontal for `left`, `right`, and `go`, or to vertical for `up` and `down`, then restores the original setting. It only writes when the requested axis differs. If it can't read or temporarily override your layout, it falls back to COSMIC's native slide behavior. Select a workspace layout in COSMIC Settings if you haven't saved one yet.

The workaround also enables horizontal slides for `move-left` and `move-right`. COSMIC doesn't animate a reorder by itself, so `cpw` uses an existing workspace as the outgoing view and returns to the moved workspace in the same Wayland transaction. You may see that neighboring workspace briefly during the slide, but the moved workspace is still active afterward. If there's no suitable workspace on the needed side, the move still works without a slide.

There is no compositor acknowledgement: the helper gives the setting 60 ms to take effect and waits 230 ms after switching before processing another move. COSMIC still chooses the slide's direction from its flat workspace order; use `cpw sync-order` if a project comes in from the wrong side. Other COSMIC controls can see the temporary layout. Avoid changing workspace settings during a switch.

Ctrl+C lets the helper finish restoring the setting. If it is forcibly killed mid-switch, use COSMIC Settings → Desktop → Workspaces to reselect your preferred layout. To disable the workaround, start the helper with `./target/release/cpw serve --no-experimental-slide`. Your shortcuts and project banners still work; `cpw` leaves the slide axis alone and transfers don't play a slide.

## Shortcuts and pinning

- **Optional shortcuts:** Add custom commands under COSMIC Settings → Keyboard → Keyboard Shortcuts. Use the **absolute path** to the built binary, followed by `left`, `right`, `up`, `down`, `move-left`, or `move-right`. Choose unused shortcuts at first so COSMIC's existing navigation remains available. The helper must be running for these commands to work.
- **Keep workspaces:** Pin a workspace in COSMIC's Workspaces overview before or after assigning it. This keeps it from disappearing when empty and lets COSMIC give the helper a stable ID. The helper saves those assignments and their last-used state in `$XDG_CONFIG_HOME/cosmic-project-workspaces/layout.json` (normally under `~/.config`). Unpinned dynamic workspaces are normally session-only: if COSMIC removes one, the helper drops its assignment. The helper does not pin or unpin anything itself.

This prototype requires a single active display. It borrows COSMIC's dynamic spare rather than creating workspaces directly. It does not move windows, change touchpad gestures, or replace COSMIC's overview. To stop, press Ctrl+C in the terminal running `serve`.
