# Sidebar usage indicator integration (#827)

This branch tracks the complete feature; keep its upstream PR in draft until
both review slices have been reviewed and integrated. No merge is requested yet.

## Review order

1. **Preference and quota selection:** additive default-compatible preference,
   conservative remaining quota across selected local logins, unit tests.
2. **Desktop integration:** Appearance dropdown, footer metric and tooltip,
   timestamp-based stale invalidation, localized strings, GPUI tests and docs.

Each child PR targets the preceding branch. Integrate the first child into this
tracker before retargeting/integrating the second. Only then is this tracker
ready for final upstream review. Do not merge dependent branches out of order.

## Boundaries

Keep estimated cost as the default; Hidden removes only the metric. Quota mode
uses the most constrained window among current local logins and displays `—`
for missing/stale data. Preserve menu-triggered refresh without startup token reads.

The existing account-switch shell-test failure is tracked separately in upstream
issue #859 and PR #860; no authentication or Engine changes belong in this chain.
