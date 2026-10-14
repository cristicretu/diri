# License texts

Diri's original source is licensed under the repository's [AGPL-3.0-only license](../../LICENSE). Distributed builds also include third-party dependencies governed by
their own terms.

- [GPL-3.0-or-later](GPL-3.0-or-later.txt) is included for Zed packages conservatively treated
  as GPL-3.0-or-later because their crate manifests do not carry independent
  license metadata.
- `Apache-2.0.txt` covers Apache-2.0 dependencies and the herdr-adapted manifests listed in `NOTICE`.

[`scripts/license-policy.json`](../../scripts/license-policy.json) records the
reviewed exceptions. The [license check](../../scripts/check-licenses.py) fails CI
when the dependency graph changes in a way that needs a new review.
