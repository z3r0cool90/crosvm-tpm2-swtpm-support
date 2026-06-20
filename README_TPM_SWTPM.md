# TPM2 Integration in crosvm — Technical Report

**Date:** June 19, 2026
**Engineer:** z3r0cool
**Objective:** Enable TPM2 chip inside a crosvm VM using swtpm socket backend

---

## Executive Summary

Full TPM2 support was implemented for guest VMs running on the crosvm hypervisor, using swtpm as a software TPM emulator. The project required changes to five files in the crosvm source tree, creation of a custom Linux kernel module, compilation of an existing upstream kernel module not shipped pre-built for this kernel, and development of a userspace bridge daemon.

**Final result:** Successful recognition and communication with TPM2 (`TPM2_PT_FAMILY_INDICATOR: "2.0"`) inside the guest VM, confirmed via `tpm2_getcap properties-fixed`.

This version of the report is based directly on `git diff` output against the unmodified upstream crosvm repository, so all code changes below are exact, not reconstructed from memory.

---

## Solution Architecture

```
Guest VM
  tpm2-tools
      |
  /dev/tpm0  (created by tpm_vtpm_proxy.ko)
      |
  proxy_fd  (kernel TPM stack)
      |
  vtpm_bridge3  (userspace daemon)
      |
  /dev/vtpm0  (custom tpm_virtio.ko)
      |
  virtio bus (device ID 62)
      |
Host crosvm
      |
  SwtpmBackend -> /tmp/swtpm.sock
      |
  swtpm (software TPM2 emulator)
```

---

## Phase 1: crosvm Source Code Changes

Five files were modified in the crosvm repository. All diffs below are taken directly from `git diff` against the clean checkout.

### 1. `devices/src/virtio/mod.rs`

**Change:** Made the `tpm` module public and registered the new `swtpm_backend` module.

```diff
 mod queue;
 mod rng;
 #[cfg(feature = "vtpm")]
-mod tpm;
+pub mod tpm;
 #[cfg(any(feature = "video-decoder", feature = "video-encoder"))]
 mod video;
 mod virtio_device;
 mod virtio_mmio_device;
 mod virtio_pci_common_config;
 mod virtio_pci_device;
-
+pub mod swtpm_backend;
 pub mod block;
 ...
-
+pub use swtpm_backend::SwtpmBackend;
 pub use vmm_vhost::SharedMemoryRegion;
```

**Why:** The new `SwtpmBackend` type (in `swtpm_backend.rs`) needed to be constructed from `device_helpers.rs`, which lives in a different module path (`src/crosvm/sys/linux/`). Without `pub mod tpm` and the `swtpm_backend` registration/re-export, `devices::virtio::SwtpmBackend` and `virtio::Tpm` were not visible outside the `devices` crate's internal module tree, causing compile errors when `create_swtpm_device` tried to use them.

---

### 2. `devices/src/virtio/tpm.rs`

**Change A — import:**
```diff
 use anyhow::anyhow;
 use anyhow::Context;
 use base::error;
+use base::info;
 use base::Event;
```
Added for the diagnostic `info!()` log line used while debugging (see Change D). Left in place as it is harmless and useful for future troubleshooting.

**Change B — `Worker::run()` now returns the backend instead of `()`:**
```diff
-    fn run(mut self, kill_evt: Event) -> anyhow::Result<()> {
+    fn run(mut self, kill_evt: Event) -> anyhow::Result<Box<dyn TpmBackend>> {
         ...
-                    Token::Kill => return Ok(()),
+                    Token::Kill => return Ok(self.backend),
```

**Change C — `Tpm` struct: new `reset()` implementation and updated `WorkerThread` type:**
```diff
 pub struct Tpm {
     backend: Option<Box<dyn TpmBackend>>,
-    worker_thread: Option<WorkerThread<()>>,
+    worker_thread: Option<WorkerThread<anyhow::Result<Box<dyn TpmBackend>>>>,
     features: u64,
 }
...
+    fn reset(&mut self) -> anyhow::Result<()> {
+        if let Some(worker_thread) = self.worker_thread.take() {
+            if let Ok(backend) = worker_thread.stop() {
+                self.backend = Some(backend);
+            }
+        }
+        Ok(())
+    }
```

**Why (B + C together):** The base `VirtioDevice` trait requires a `reset()` implementation; the original `tpm.rs` had none, so crosvm logged `reset not implemented for virtio-tpm` whenever the guest driver was unloaded/reloaded. Worse, on the *first* `activate()` call, `self.backend.take()` permanently consumed the backend (it was an `Option`, taken once). On any subsequent activation attempt (e.g. after the guest driver reset the device), `self.backend` was `None`, so `activate()` failed immediately with `no backend in vtpm`. By having the worker thread return the backend when it stops, and having `reset()` put that backend back into `self.backend`, the device becomes safely re-activatable.

**Change D — diagnostic log line in `activate()`:**
```diff
     ) -> anyhow::Result<()> {
+        info!(">>> TPM activate called with {} queues", queues.len());
         if queues.len() != 1 {
```
Added during debugging to confirm whether `activate()` was being called at all, and with how many queues. This was essential in isolating the queue-readiness problem from the custom guest driver (see kernel module notes below) versus a configuration problem on the host side.

**Minor formatting noise:** a stray blank line was added inside the `TpmBackend` trait body and inside the `Tpm` struct body; these have no functional effect and can be cleaned up before upstreaming.

---

### 3. `src/crosvm/cmdline.rs`

**Change A — new `--swtpm-socket` flag, added unconditionally (not gated behind any feature):**
```diff
     /// path to a socket from where to read switch input events and write status updates to
     pub switches: Vec<PathBuf>,
 
+    #[argh(option)]
+/// path to swtpm socket
+    pub swtpm_socket: Option<PathBuf>,
     #[argh(option, arg_name = "TAG")]
     /// (DEPRECATED): Use --syslog-tag before "run".
```

**Change B — root cause fix: moved the assignment out of the `vtpm` feature gate:**
```diff
         #[cfg(feature = "vtpm")]
         {
             cfg.vtpm_proxy = cmd.vtpm_proxy.unwrap_or_default();
-        }
 
-        cfg.virtio_input = cmd.input;
 
+        }
+        cfg.swtpm_socket = cmd.swtpm_socket.clone();
+        cfg.virtio_input = cmd.input;
         if !cmd.single_touch.is_empty() {
```

**Why — this was the central bug of the whole project.** In the very first working version of this code, `cfg.swtpm_socket = cmd.swtpm_socket.clone();` had been placed *inside* the `#[cfg(feature = "vtpm")]` block alongside `cfg.vtpm_proxy`. Both the flag *definition* and the flag *assignment* were originally feature-gated. Since `swtpm_socket` is conceptually unrelated to the ChromeOS `vtpm`/D-Bus feature — it's a plain Unix socket backend that works on any Linux host — gating it behind that feature meant `cfg.swtpm_socket` was silently `None` at runtime even when `--swtpm-socket /path` was passed on the command line and even when the binary was built with `--features vtpm`. As a result, `create_swtpm_device()` (in `linux.rs`) was simply never called, and the guest never saw a TPM device on the virtio bus, with **no error printed anywhere**. This was the hardest bug to find precisely because it failed silently. Diagnosis required adding an `info!()` log immediately after the assignment and confirming it never printed.

**Minor formatting noise:** a stray blank line was added near `acpi_table` and after `vtpm_proxy`; no functional effect.

---

### 4. `src/crosvm/config.rs`

**Change:** Added the `swtpm_socket` field to the `Config` struct and its `Default` impl.

```diff
     pub sve: Option<SveConfig>,
     pub swap_dir: Option<PathBuf>,
     pub swiotlb: Option<u64>,
+    /// Path to swtpm Unix socket for TPM emulation
+    pub swtpm_socket: Option<PathBuf>,
     #[cfg(target_os = "android")]
     pub task_profiles: Vec<String>,
...
             sve: None,
             swap_dir: None,
             swiotlb: None,
+            swtpm_socket: None,
             #[cfg(target_os = "android")]
```

**Why:** Straightforward plumbing — the field needs to exist on `Config` (unconditionally, i.e. not feature-gated, matching the fix in `cmdline.rs`) for `cmdline.rs` to populate it and for `linux.rs` to read it.

---

### 5. `src/crosvm/sys/linux.rs`

**Change A — import the new device constructor:**
```diff
+use crate::crosvm::sys::linux::device_helpers::create_swtpm_device;
 use crate::crosvm::sys::platform::vcpu::VcpuPidTid;
```

**Change B — call `create_swtpm_device` unconditionally when a socket path is configured, and make the existing ChromeOS vtpm-proxy path mutually exclusive with it:**
```diff
+    // swtpm socket backend (works on any Linux, not just ChromeOS)
+    if let Some(ref socket_path) = cfg.swtpm_socket {
+        info!(">>> swtpm_socket path: {:?}", socket_path);
+        devs.push(create_swtpm_device(
+            cfg.protection_type,
+            cfg.jail_config.as_ref(),
+            Path::new(socket_path),
+        )?);
+        info!(">>> swtpm device created successfully");
+    }
     #[cfg(feature = "vtpm")]
     {
-        if cfg.vtpm_proxy {
+    // ChromeOS vtpm-proxy (requires D-Bus daemon)
+        if cfg.vtpm_proxy && cfg.swtpm_socket.is_none() {
             devs.push(create_vtpm_proxy_device(
                 cfg.protection_type,
                 cfg.jail_config.as_ref(),
```

**Why:** This is the actual device-construction call site. The `swtpm_socket` branch is placed outside any `#[cfg(feature = "vtpm")]` guard so it compiles and works on builds without the `vtpm` feature enabled. The existing ChromeOS `vtpm_proxy` branch was given an additional `&& cfg.swtpm_socket.is_none()` condition so the two backends cannot both attempt to register a TPM device when both are configured. The two `info!()` lines were added for debugging and confirmed (a) that the flag was reaching this function, and (b) that `create_swtpm_device` returned successfully — both were essential in isolating the `cmdline.rs` bug from the kernel-side queue-activation bug described later.

**Minor formatting noise:** a stray blank line was removed/added near the top doc comment and near the `x86_64::X8664arch` import; no functional effect.

---

### 6. `src/crosvm/sys/linux/device_helpers.rs`

**Change A — import the new backend type:**
```diff
 use devices::virtio::vhost_user_backend::VhostUserVsockDevice;
+use devices::virtio::swtpm_backend::SwtpmBackend;
 use devices::virtio::vsock::VsockConfig;
```

**Change B — new `create_swtpm_device` function:**
```diff
+/// Creates a virtio TPM device using swtpm socket backend (for non-ChromeOS systems)
+pub fn create_swtpm_device(
+    protection_type: ProtectionType,
+    jail_config: Option<&JailConfig>,
+    socket_path: &Path,
+) -> DeviceResult {
+    let jail = simple_jail(jail_config, "tpm_device")?;
+
+    let backend = SwtpmBackend::new(socket_path)
+        .context("failed to connect to swtpm socket")?;
+
+    let dev = virtio::Tpm::new(Box::new(backend), virtio::base_features(protection_type));
+    Ok(VirtioDeviceStub {
+        dev: Box::new(dev),
+        jail,
+    })
+}
```

**Why:** This is the constructor that ties everything together — it opens a Unix socket connection to swtpm via `SwtpmBackend::new()`, wraps it in a `virtio::Tpm` device, and returns a `VirtioDeviceStub` ready to be added to the VM's device list. An earlier iteration of this function had a redundant, circular `use` statement inside its body (`use crate::crosvm::sys::linux::device_helpers::SwtpmBackend;`, importing from its own module) which was removed; the import shown above at the top of the file is the correct, final version.

---

## Summary Table of crosvm Repository Changes

| File | Type of change | Root cause addressed |
|------|------|------|
| `devices/src/virtio/mod.rs` | Made `tpm` module public; added `swtpm_backend` module + re-export | Visibility error: `SwtpmBackend`/`Tpm` not reachable from `device_helpers.rs` |
| `devices/src/virtio/tpm.rs` | Added `reset()`; worker thread returns backend; added debug log | Device could not be re-activated after first use (`no backend in vtpm`) |
| `src/crosvm/cmdline.rs` | Added `--swtpm-socket` flag unconditionally; moved assignment out of `#[cfg(feature="vtpm")]` | **Root cause:** flag value was silently always `None` at runtime |
| `src/crosvm/config.rs` | Added `swtpm_socket: Option<PathBuf>` field | Plumbing required by the cmdline.rs fix |
| `src/crosvm/sys/linux.rs` | Call `create_swtpm_device` unconditionally; made vtpm-proxy mutually exclusive | Device was never constructed even when the config field was correctly set |
| `src/crosvm/sys/linux/device_helpers.rs` | New `create_swtpm_device()` function; fixed import | Provides the actual device construction logic |

---

## Phase 2: Guest Kernel Diagnosis

### Problem: Kernel / Modules Mismatch

crosvm was initially booted with the host kernel (`6.17.0-35-generic`), but the guest rootfs only contained kernel modules for `6.8.0-117-generic`. The kernel booted, but had no modules directory matching its own version, so it could not load any TPM-related driver even if one existed.

**Fix:** Mounted the boot partition of the guest disk image directly and booted crosvm using the guest's own matching kernel:

```bash
sudo fdisk -l noble-server-cloudimg-amd64.raw
# Boot partition starts at sector 227328; Linux filesystem at sector 2099200

sudo losetup -f --offset $((227328 * 512)) noble-server-cloudimg-amd64.raw
sudo mount /dev/loopXX /mnt/vm-boot

crosvm run ... /mnt/vm-boot/vmlinuz-6.8.0-117-generic
```

### Problem: Non-Standard Virtio TPM Device ID

Inspection of `virtio_sys/src/virtio_ids.rs` confirmed crosvm defines:

```rust
pub const VIRTIO_ID_TPM: u32 = 62;  // non-standard; official virtio spec assigns 29
```

Neither the `6.8.0-117-generic` nor `6.17.0-35-generic` Ubuntu kernels ship with `CONFIG_TCG_VIRTIO_VTPM` enabled (confirmed via `grep` on both `/boot/config-*` files), and even if they had, the standard in-tree driver expects device ID 29, not 62. A custom out-of-tree kernel module was therefore required to recognize crosvm's virtio TPM device.

---

## Phase 3: Custom Kernel Module — `tpm_virtio.c` / `tpm_virtio.ko`

Written from scratch (no upstream equivalent targets ID 62). Responsibilities:
- Registers as a virtio driver matching device ID 62
- Negotiates the single virtqueue and calls `virtio_device_ready()` (required for crosvm's `activate()` to see the queue as ready — without it, crosvm's `Tpm::activate()` received 0 queues instead of 1, producing the `expected 1 queue, got 0` error)
- Exposes a blocking-I/O `/dev/vtpm0` misc character device using kernel `completion` primitives to synchronize userspace read()/write() calls with virtqueue request/response cycles

See attached `tpm_virtio.c` for the full, final, working source.

**Build:**
```bash
make -C /lib/modules/$(uname -r)/build M=$(pwd) modules
sudo insmod tpm_virtio.ko
```

---

## Phase 4: Upstream Kernel Module — `tpm_vtpm_proxy.ko`

This is **not custom code**. `tpm_vtpm_proxy.c` is the standard upstream Linux driver at `drivers/char/tpm/tpm_vtpm_proxy.c`, present in every kernel source tree but not pre-built as a loadable module for this particular Ubuntu kernel build. It was compiled manually from the matching `linux-source-6.8.0` package (see attached `README_tpm_vtpm_proxy.md` for the exact reproduction steps).

It exposes `/dev/vtpmx`; issuing the `VTPM_PROXY_IOC_NEW_DEV` ioctl against it (done by the bridge daemon below) creates the actual `/dev/tpm0` device that standard tools (`tpm2_getcap`, `tpm2-tools`, `tpm2-abrmd`) expect.

**Build note:** it had to be compiled in a separate directory from `tpm_virtio.ko` — building both `.o` files together in the same module directory produced a `missing MODULE_LICENSE()` modpost error from stale build artifacts.

---

## Phase 5: Userspace Bridge Daemon — `vtpm_bridge3.c`

Written from scratch. Bridges the kernel TPM stack (via the proxy file descriptor returned by the `VTPM_PROXY_IOC_NEW_DEV` ioctl) and the custom virtio device at `/dev/vtpm0`:

```c
// 1. Request a new /dev/tpmN device from the proxy driver
struct vtpm_proxy_new_dev dev = { .flags = VTPM_PROXY_FLAG_TPM2 };
ioctl(vtpmx_fd, VTPM_PROXY_IOC_NEW_DEV, &dev);

// 2. Forward TPM commands and responses between the two file descriptors
while (1) {
    n = read(dev.fd, buf, BUFSIZE);     // command from kernel TPM stack
    write(vtpm0_fd, buf, n);            // -> virtio -> crosvm -> swtpm
    r = read(vtpm0_fd, buf, BUFSIZE);   // response from swtpm
    write(dev.fd, buf, r);              // -> back to kernel TPM stack
}
```

See attached `vtpm_bridge3.c` for the full source.

### Problem: swtpm Locality Support

The Linux kernel TPM driver issues a locality-set request as part of its startup sequence. The initial swtpm invocation rejected this (`tpm tpm0: A TPM error (257) occurred attempting to set locality`), which caused the bridge daemon's read on `/dev/vtpm0` to return immediately with EOF/broken pipe.

**Fix:**
```bash
swtpm socket \
  --tpmstate dir=/tmp/mytpm \
  --server type=unixio,path=/tmp/swtpm.sock \
  --ctrl type=unixio,path=/tmp/swtpm.ctrl.sock \
  --tpm2 \
  --flags startup-clear \
  --locality allow-set-locality \
  --daemon
```

---

## Final Results

```
$ sudo tpm2_getcap properties-fixed
TPM2_PT_FAMILY_INDICATOR:
  raw: 0x322E3000
  value: "2.0"
TPM2_PT_LEVEL:
  raw: 0
TPM2_PT_REVISION:
  raw: 0xA4
  value: 1.64
TPM2_PT_DAY_OF_YEAR:
  raw: 0x4B
```

Full bidirectional TPM2 command/response communication confirmed between guest userspace and swtpm via the virtio transport layer end to end.

---

## Artifacts Produced / Used

| File | Origin | Description |
|------|--------|-------------|
| `tpm_virtio.c` / `.ko` | Written from scratch | Custom kernel module for virtio TPM device ID 62 |
| `tpm_vtpm_proxy.c`, `tpm.h` | Upstream Linux kernel source (not authored by us) | Compiled because no pre-built module shipped for this kernel; exposes `/dev/tpm0` |
| `vtpm_bridge3.c` / binary | Written from scratch | Userspace daemon bridging kernel TPM stack and virtio device |
| crosvm repo diff (6 files) | Modified upstream | See detailed diffs above |

---

## Recommendations / Next Steps

1. **Persistent setup:** Create a systemd service to automatically load `tpm_vtpm_proxy.ko` and `tpm_virtio.ko`, and start `vtpm_bridge3` on boot.
2. **Upstream alignment:** Either change `VIRTIO_ID_TPM` in crosvm to the official virtio spec value (29) so the in-tree Linux `tpm_tis_virtio`-style drivers could eventually apply, or formally propose `tpm_virtio.ko` upstream with crosvm's ID documented as vendor-specific.
3. **Kernel config:** Build a custom guest kernel with `CONFIG_TCG_VIRTIO_VTPM=m` (if/once such a driver targets ID 62) to remove the dependency on an out-of-tree module entirely.
4. **Packaging:** Produce a `.deb` (or DKMS package) bundling `tpm_virtio.ko`, the compiled `tpm_vtpm_proxy.ko`, and the `vtpm_bridge3` binary so they survive guest kernel upgrades.
5. **Security:** Re-enable the crosvm sandbox (`--disable-sandbox` was used only for debugging) and define an appropriate seccomp policy for the TPM device jail before any production use.
6. **Code cleanup before upstreaming:** remove stray blank lines and the commented-out `//use super::swtpm_backend::SwtpmBackend;` line in `tpm.rs` introduced during development; these have no functional effect but should not be part of a clean PR.
