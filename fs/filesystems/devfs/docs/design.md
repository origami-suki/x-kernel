# devfs design

Devfs builds the boot `/dev` namespace from registered device operation tables.
The `nodes/dri.rs` publisher uses `drmdevice::available` and one registered Card0
per actual display device. Primary `card0` uses device number 226:0; healthy
implemented rendering additionally publishes `renderD128` at 226:128, sharing
that Card0 via RenderNode. Separate opens attach separate private state, while
fd duplication retains the VfsFile description. RenderNode rejects KMS/master
operations before user access; the devfs publisher does no GPU waits.

Node publication is a boot-time snapshot. Runtime hotplug namespace rebuilding
is not implemented by this change. Driver-owned allocations and file/mapping
lifetimes remain the responsibility of the installed DRM operation table.
