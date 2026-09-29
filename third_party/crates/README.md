# Crate license texts

`scripts/gen_notices.py` puts the license files of every crate in the normal
dependency graph of each shipped binary into `THIRD-PARTY-NOTICES.txt`
(ROADMAP §10.9, DESIGN §1.5). It reads them from the crate's registry source:
the `license-file` its manifest names, and its top-level `LICENSE*`,
`LICENCE*`, `COPYING*`, `NOTICE*`, `COPYRIGHT*` and `UNLICENSE*` files.

A crate that ships none gets its text here, at

    third_party/crates/<name>-<version>/<file>

copied from the crate's upstream repository at the tag of that version, and
`gen_notices.py` takes every file in that directory. A version bump needs a new
directory. `gen_notices.py` fails and names the crate when neither exists.

No crate needs one today.
