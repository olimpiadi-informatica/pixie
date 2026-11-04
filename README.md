# pixie v2

A system to network-boot and (re-)image many machines over a LAN. Machines
PXE-boot into a small UEFI client, register themselves, and can then be told
to *store* (capture) or *flash* (restore) a disk image. Restores are
rsync-like: a client first diffs against its own local disk and only ever
fetches genuinely missing data, and the server distributes that missing data
by UDP broadcast (with forward-error-correction) so many machines pulling the
same image share a single transfer instead of each downloading it
separately. This makes it practical to re-image a whole room of machines
(e.g. a computer lab or a programming-contest venue) quickly and repeatedly.

## How it works

pixie is made of six crates:

- **`pixie-server`** — the coordinator, run as root on one machine. It runs
  built-in DHCP/PXE, TFTP, and DNS servers, an admin HTTP/SSE API, and the
  UDP protocol that clients speak.
- **`pixie-client`** — the client agent (`pixie-client.efi`), a bare-metal
  application that each machine PXE-boots into. It talks to the server and
  executes the actions it is given (register, store, flash, boot, restart,
  shutdown).
- **`pixie-kernel`** — the bare-metal x86_64 kernel runtime, memory manager,
  async executor, and network stack used by `pixie-client`.
- **`pixie-drivers`** — `#![no_std]` async hardware drivers (NVMe, AHCI,
  Intel e1000/e1000e/igc, Realtek 8168/8169/8125, USB Ethernet RTL8152/8153/8156
  & CDC-ECM/NCM, xHCI, PS/2, Serial, PCI, ACPI).
- **`pixie-web`** — a WASM admin dashboard (built with `trunk`), served by
  `pixie-server`, for watching machine status and issuing actions.
- **`pixie-shared`** — wire-protocol types and the chunk codec shared by the
  crates above.

Typical workflow:

1. **Register** — boot a blank machine; on-screen you pick its group, row and
   column and which image it should track. It's assigned a static IP of the
   form `10.{group}.{row}.{col}`.
2. **Store** (push) — the client parses the disk's partitions
   (filesystem-aware for FAT/ext4/NTFS/swap, so only used data is read),
   content-hashes each chunk, and uploads only the chunks the server doesn't
   already have.
3. **Flash** (pull) — the client fetches the target image's manifest, first
   scans its *own* disk for chunks it already has (no network transfer for
   unchanged data), and requests only what's still missing. The server
   answers by broadcasting each requested chunk once to the whole subnet, so
   every machine waiting on it receives it from a single transfer.

Admin actions (assign images, rebuild/rollback/delete image versions,
garbage-collect unreferenced chunks, act on a single machine or a whole
group/image at once) are available from the web dashboard or the server's
`/admin/...` HTTP API.

## Prerequisites

* Install the required dependencies
  ```sh
  yay -S rustup upx trunk
  rustup toolchain install stable
  rustup target add wasm32-unknown-unknown x86_64-unknown-uefi
  ```
* `pixie-server` must run as root (it needs device-bound sockets and
  privileged ports 53, 67, and 69 for DNS, DHCP, and TFTP).

## Quickstart

* run `./setup.sh` to compile pixie and prepare the `storage` directory.
  Pass `--release` to build in release mode (and UPX-compress the UEFI
  binary), and optionally a path to use instead of `./storage`.
* modify the configuration file at `storage/config.yaml`.
* run with root privileges the server: `sudo ./pixie-server/target/release/pixie-server -s storage`
  (`-s`/`--storage-dir` defaults to `./storage`).

## Configuration

`storage/config.yaml` (see `pixie-server/example.config.yaml` for a starting
point) has the following top-level sections:

* `hosts.interfaces` — one entry per network interface pixie should manage,
  each with:
  * `network` — the interface's network, e.g. `10.0.0.1/8`.
  * `dhcp` — either `!static [low, high]`, meaning pixie runs its own DHCP
    server over that IP range, or `!proxy <ip>`, meaning pixie only answers
    PXE requests (ProxyDHCP) alongside another DHCP server already on the
    LAN, given as `<ip>`.
  * `broadcast_speed` — rate limit, in bytes/second, for chunk broadcasts on
    that interface.
* `hosts.hostsfile` — optional path to an `/etc/hosts`-style file used to
  give registered machines friendly hostnames.
* `hosts.dns_upstream` — whether `pixie-server`'s built-in DNS server should
  forward unknown DNS queries upstream, or only answer for pixie's own hosts.
* `http.listen_on` — address:port the admin dashboard/API is served on.
* `groups` — a list of `[name, id]` pairs mapping human-readable group names
  (e.g. rooms) to the numeric id used in the static IP scheme.
* `images` — the list of image names machines can be assigned to track.

## Production install

`install.sh` installs pixie system-wide: it builds a release build into
`/var/local/lib/pixie`, copies the `pixie-server` binary to
`/usr/local/bin`, and installs the provided `pixie.service` systemd unit.
After running it:

```sh
sudo systemctl enable --now pixie
```
