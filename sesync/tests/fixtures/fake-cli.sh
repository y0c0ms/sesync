#!/bin/sh
# The one executable behind every fake CLI in the sesync tests. `fake_cli::stage`
# (tests/support/fake_cli.rs) links a fake's path here and writes the fake's script to
# `<path>.body`; this runs that script with sh. Committed with the exec bit and never written
# by a test, so no test can hit ETXTBSY on it. See fake_cli.rs for why.
exec /bin/sh "$0.body" "$@"
