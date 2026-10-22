# Desktop window: target layout

The window follows the *workflow* of classic multi-link download managers: collect
links, review them, run a queue of packages, watch totals. This page describes the
layout in our own words. **Nothing is copied**: no icons, pictures, logos, themes,
texts or translations of any other program. Our icons are drawn by us, our texts come
from the i18n catalog, and our name and colours are our own (see "Clean-room rules"
in `AGENTS.md`).

Derived from a reference screenshot supplied by the project owner (kept locally, not
in this repository).

## Frame (top to bottom)

1. **Toolbar** with icon buttons, in groups:
   - run control: start all, pause all, stop all;
   - queue order: move selection to top, up, down, to bottom;
   - switches (toggle buttons): watch clipboard, reconnect on IP block (not needed over Tor, so omitted), use premium accounts, and similar session-wide options;
   - clear finished entries.
2. **Tabs**: *Downloads*, *Link collector*, *History* (our extra). Free space to the right of the tabs stays empty (no advertising).
3. **Table** that fills the window. Columns, each sortable, resizable and individually
   hideable through a column menu at the right end of the header:
   Name, Size, Host, Connection, Status, Speed, ETA, Loaded, Progress, Save to, Comment.
   Rows are **packages** (collapsible groups) containing **links**.
4. **Overview panel** (collapsible, closable) with totals: packages, links, total size,
   loaded, download speed, remaining bytes, time left, running downloads, open connections.
5. **Command bar**: a prominent *Add links* button (with a menu), a search box with a
   scope selector (file name, host, ...), and a filter menu (all, running, finished, ...).
6. **Footer line**: shortcut to add an account, small status indicators at the right.

## Behaviour to reproduce (from observation)

- Empty state: header and columns are visible with no rows; totals read zero.
- Column header offers sorting, drag to reorder, width lock, and a column chooser.
- Search filters the visible rows live; the filter menu narrows by state.
- Totals update continuously while downloads run.
- Everything in the frame works with the keyboard and a context menu on rows
  (start, stop, open folder, remove, move, rename package, copy link).

## Our additions and differences

- Tor status and per-download circuit health next to the totals.
- A *History* tab with statistics and export.
- No advertising, no donation tab, no reconnect feature (Tor changes the exit instead).
- English interface through the string catalog, translatable without code changes.

## Build order

1. `gui-2` frame: toolbar, tabs, overview panel, command bar, footer (this page, items 1, 2, 4, 5, 6).
2. `gui-3` table: columns, sorting, column chooser, packages (data model with a package
   per download group) and the extra fields the columns need (save path, comment).
3. `gui-4` link collector tab on top of the existing collector model.
4. `gui-5` dialogs and menus: add-links dialog, row context menu, settings dialog, file picker.
5. `gui-6` app package: macOS `.app`, icon, Windows/Linux packages, autostart.
