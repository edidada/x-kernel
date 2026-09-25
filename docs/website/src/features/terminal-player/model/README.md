# Terminal demo content

The `boot` tab uses excerpts from a captured session from
`assets/recordings/x-kernel/boot/session.json`. Its source logs and capture details
are in that directory. The checked-in recording files are the source of truth; regenerate them with your local tooling when needed;
`bootRecording.ts` converts it to accelerated playback events with selected configuration, build and boot output. ANSI control codes are removed for the plain text player.

The configure, build and explore tabs remain curated demos rather than recordings.
Their commands come from `x-kernel/README_CN.md`; the relinking message comes from
`xtask/xkmake/src/build.rs`. Explore uses shell builtins in the guest `/root`.
