# Dashboard design

The dashboard is a workspace for exploring and comparing recorded experiments.
Its visual system uses flat neutral surfaces, emerald accents, compact spacing,
and consistent controls across history, inspection, storage, files, logs, and login.

## Layout and navigation

The header contains project or source selection, native Runs / Storage radio
navigation, appearance, refresh, and session actions. Small project catalogs use
selectable tags; larger catalogs use a select. Header controls wrap on smaller
screens without occupying a separate navigation rail.

Full run history occupies a persistent left panel above 1000 CSS pixels, with
inspection, comparison, or Storage in the remaining space. The panels have
independent vertical scrolling and keep pagination reachable at short viewport
heights. Below that width, the panels stack. Explicit inspection and comparison
move focus and the viewport
to the review on compact screens. Closing an inspection returns to the run
browser. Background refresh and checkbox selection preserve focus and position.

Run and status are the first data columns, followed by machine when applicable,
task, and start time. Optional parameter and metric columns scroll within the
table, with selection and run identity pinned during horizontal scrolling.
History shows two tags and an additional-tag count, with the full list in
inspection. Filters remain visible native controls, including status, history, and
machine choices. Counts always describe the actual server result rather than
invented workspace totals.

## Appearance and accessibility

Light is the default. Dark and System are available through native radio
controls; the browser stores only the appearance preference under
`expri.dashboard.theme`. Unavailable browser storage falls back to Light while
still allowing appearance changes on the current page.

The existing `app.js` asset is a classic production bundle loaded before CSS.
It restores appearance before rendering and mounts the dashboard once the DOM
is ready. This keeps the existing four-file deployment manifest compatible with
older AB servers. The login page remains scriptless with a native password POST
form. Login and standalone chart reports follow the system color scheme.

Controls have visible focus indicators and retain native radio and checkbox
keyboard behavior. Review and log tabs retain roving arrow-key navigation.
Compact navigation scrolling respects reduced motion. Long names and IDs wrap
without forcing page overflow; wide tables, charts, and logs own their scrolling
regions.

## Review and capability states

Charts, Overview, Logs, and Files share the same inspection workspace. Nickname
and tag editing uses a collapsed native disclosure so charts have more space.
The compact inspection header places run identity, status, and metadata together.
Metric panels use a responsive grid and show the latest recorded value beside
each title for single-run inspection.
The editor remains mounted, preserving drafts and save errors through refresh
and tab changes. Unsupported servers show a visible unavailable state; read-only
previews keep mutation controls disabled.

The chart controller owns the contents of `#chart-preview`. That element remains
mounted across inspection, comparison, source, tab, and storage changes. Metric
picker and log nodes also retain their existing controller boundaries. The
redesign preserves run identity, retention behavior, and editor drafts. Because
history remains visible in Storage, storage update polling also refreshes history
when its revision changes, with a bounded fallback for older servers. It does
not fetch hidden inspection details, charts, or logs.

## Verification

Run the frontend type check, production build, and regression suite described in
the README. Bootstrap tests exercise classic bundle syntax, early appearance
restoration, native login, and one-time mounting. Dashboard integration tests
cover compact navigation, persistent charts, annotation disclosure, and themes
without data reloads. Rust dashboard and server tests check embedded assets,
scriptless reports, authentication, and versioned AB assets.

Visual verification uses synthetic local fixtures in Firefox at desktop,
tablet, and phone widths, including light/dark appearance, inspection,
comparison, storage, files, login, and old-server/empty/error states.
