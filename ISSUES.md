# Issues

## After a server crash, writes stall about 30 s even though the client reconnects at once

A request in flight when the connection dies is resent only when it times out
(`REQUEST_TIMEOUT` in `backend/crates/nbd-client/src/main.rs`):
`block nbd0: Connection timed out, retrying (1/1 alive)` about 30 s after the drop. Nothing
is lost and the root stays read-write. A normal restart no longer does this, because the
server answers what it was sent before it exits (`Live::drain`); a killed or crashed
server still does.

A shorter timeout isn't free. It would make the kernel reconnect during a slow request,
and the old connection's write can still be running, so it could land after a newer write
to the same block. `connection()` would have to wait for it before the export is claimed
again, and the timeout is baked into each image's initramfs.
