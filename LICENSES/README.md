# License texts

Diri's original source is licensed under the repository's Apache-2.0
`LICENSE`. Distributed builds also include third-party dependencies governed by
their own terms.

- `GPL-3.0-or-later.txt` is included for Zed packages conservatively treated
  as GPL-3.0-or-later because their crate manifests do not carry independent
  license metadata.
- Apache-2.0 dependencies are covered by the repository's Apache-2.0 text.

`license-policy.json` records the reviewed exceptions and
`scripts/check-licenses.py` fails CI when the dependency graph changes in a way
that needs a new review.
