# Issues

## Writes stall about 30 s after an NBD server restart, though the client reconnects at once

Seen 2026-10-30 4:42 PM PDT (23:42Z), on a VM with its root on the uberfall store, twice
out of two `docker restart bootbob-store` under `date >> file; sync` every 0.5 s. The
client logs `reconnected` within about 1.5 s, then nothing is written for 31 s, then
`block nbd0: Connection timed out, retrying (1/1 alive)` and writes carry on. Nothing is
lost: no I/O errors, root stays rw, a test file's hash is unchanged.

The dmesg timing points at `REQUEST_TIMEOUT` (30 s) in `backend/crates/nbd-client/src/main.rs`:
it looks like a request in flight when the connection dies is only requeued when it
times out. Any machine with I/O in flight during a restart pauses that long, so this
applies to the main server too (the earlier 0.5 s figures measured the reconnect, and
the tests wrote too rarely to catch it reliably). `tools/test-pi-golden.sh` and any
"survives a restart" check should bound the longest gap between writes, not only check
that writes resume. Clients are baked into golden images and initramfses, so a fix
reaches a machine only after a rebuild.
