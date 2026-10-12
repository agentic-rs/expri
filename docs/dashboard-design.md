# Dashboard design

The dashboard is a workspace for exploring and comparing recorded experiments.
Its visual system uses neutral surfaces, emerald accents, readable typography,
and consistent controls across runs, review, storage, files, logs, and login.

## Layout and navigation

The sidebar contains native Runs / Storage radio navigation, project or source
selection, and appearance controls. Small project catalogs use selectable tags;
larger catalogs use a select. Refresh controls and session actions stay in the
header. The sidebar becomes a compact navigation area on smaller screens.

Run browsing uses the full workspace width before inspection. Opening a run or
comparison creates a sibling review pane above 1180 CSS pixels; below that width,
the panes stack. Explicit inspection and comparison move focus and the viewport
to the review on compact screens. Closing an inspection returns to the run
browser. Background refresh and checkbox selection preserve focus and position.

Run and status are the first data columns, followed by machine when applicable,
task, and start time. Optional parameter and metric columns scroll within the
table. Filters remain visible native controls, including status, history, and
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
The editor remains mounted, preserving drafts and save errors through refresh
and tab changes. Unsupported servers show a visible unavailable state; read-only
previews keep mutation controls disabled.

The chart controller owns the contents of `#chart-preview`. That element remains
mounted across inspection, comparison, source, tab, and storage changes. Metric
picker and log nodes also retain their existing controller boundaries. The
redesign changes presentation without changing run identity, API requests,
retention behavior, or request scheduling.

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
