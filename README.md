# Wake

A tiny home-lab appliance for a dual-boot PC. From your phone, pick **Linux** or **Windows** and tap **Wake**. Wake sends a Wake-on-LAN magic packet. While the PC boots, GRUB asks Wake which system to start, boots it once, and then goes back to Linux.

If Wake can't be reached for any reason (container down, network down, timeout, garbage reply), GRUB boots Linux as it normally would.

One Rust binary, one container, one small volume. It has no database, no queue and no cloud.

---

## Contents

1. [What Wake does](#1-what-wake-does)
2. [Architecture](#2-architecture)
3. [Requirements](#3-requirements)
4. [Building](#4-building)
5. [Portainer deployment](#5-portainer-deployment)
6. [Configuration](#6-configuration)
7. [Finding the PC's MAC address](#7-finding-the-pcs-mac-address)
8. [Testing Wake-on-LAN](#8-testing-wake-on-lan)
9. [Installing the GRUB integration](#9-installing-the-grub-integration)
10. [Required GRUB modules](#10-required-grub-modules)
11. [Testing a Linux boot](#11-testing-a-linux-boot)
12. [Testing a Windows boot](#12-testing-a-windows-boot)
13. [One-shot boot behaviour](#13-one-shot-boot-behaviour)
14. [Troubleshooting Wake-on-LAN](#14-troubleshooting-wake-on-lan)
15. [Troubleshooting Docker networking](#15-troubleshooting-docker-networking)
16. [Troubleshooting GRUB networking](#16-troubleshooting-grub-networking)
17. [Security considerations](#17-security-considerations)

Also: [API](#api) · [Tests](#tests) · [Project layout](#project-layout)

---

## 1. What Wake does

- **Web page (phone first).** Shows whether the PC is off, waking, booting or on, and what it will boot next. One tap picks the OS; one tap wakes the PC. You can install it to the home screen as a PWA.
- **REST API.** `curl -X POST http://SERVER:8080/api/wake/windows` picks Windows and wakes the PC in one call.
- **Wake-on-LAN.** Sends standard magic packets as a UDP broadcast.
- **One-shot boot choice for GRUB.** Answers GRUB's request during boot with `0` (Linux) or `1` (Windows) and then forgets the choice.
- **Status.** Checks whether the PC answers on the network (ping and/or TCP ports you choose) and keeps a short log of events.

## 2. Architecture

```text
 Phone ──HTTP :8080──▶ ┌──────────────────────── Wake (one Rust process) ───────────────────────┐
                       │  web page + REST API + SSE      ──▶ BootService (one mutex, one file)  │
                       │  WOL sender  ── UDP broadcast :9 ──────────────────────────────▶ LAN    │
                       │  status monitor (icmp / tcp probes to PC_IP)                           │
                       │  GRUB responder :8081  ◀── GET /grub/boot.env ───────────┐              │
                       └───────────────────────────────────────────────────────────┼──────────────┘
                                                                                    │
 PC: power on ─▶ UEFI ─▶ GRUB ─▶ efinet + net_dhcp ─▶ load_env (http,SERVER:8081)/grub/boot.env
                                  │
                                  ├─ wake_boot=1  ─▶ chainload Windows Boot Manager
                                  └─ anything else ─▶ Linux (the default)
```

### Why GRUB talks HTTP and reads an "environment block"

This is the unusual part, so here is what GRUB 2 on x86_64 EFI can and can't do (checked against GRUB 2.14's manual and its `net/http.c`, `net/tftp.c`, `net/tcp.c` source):

- GRUB has **only two network protocols: TFTP and HTTP** (`tftp.mod`, `http.mod`). There is no raw UDP, no raw TCP and no TLS. The network devices look like `(http,SERVER:PORT)/path` and `(tftp,SERVER:PORT)/path`.
- It reaches the NIC through the **firmware's UEFI network driver** (`efinet.mod`, over SNP), and gets an address with `net_dhcp` or `net_add_addr`.
- **GRUB can't read a file's contents into a variable.** There is no `read` or `cat`-into-variable command. What it can do is `load_env`, which imports variables from a *GRUB environment block* (the format of `/boot/grub/grubenv`). Given a whitelist, it imports only the names you list.

So GRUB runs:

```text
load_env --skip-sig --file (http,SERVER:8081)/grub/boot.env wake_boot
```

Wake answers with a 1024-byte environment block:

```text
# GRUB Environment Block
wake_boot=1
##########...   (padded with '#' to 1024 bytes, exactly like grub-editenv writes)
```

**Why HTTP instead of TFTP?** When the server doesn't answer, GRUB's TCP connect gives up after about 16 s (`GRUB_NET_TRIES` 40 × 400 ms). A TFTP open retries 40 times with a growing interval, about 32 s. HTTP also needs no UDP server or TFTP option negotiation, works on any port, and can be tested with `curl`.

**Why a hand-written responder?** GRUB's HTTP client checks `HTTP/1.1 ` and `Content-Length: ` with a *case-sensitive* `memcmp`, and most Rust HTTP stacks (hyper) write header names in lowercase. The GRUB port is a small separate listener that writes exactly the bytes GRUB expects. It serves only two paths.

**Why `load_env` and not `source`?** `source` would execute whatever the network sends as a GRUB script. `load_env` with a one-name whitelist can only ever set `wake_boot`, and only the exact value `1` changes anything.

### Protocol reference

| | |
|---|---|
| Transport | HTTP/1.1 over TCP, port `GRUB_PROTOCOL_PORT` (default 8081), separate from the web port |
| Request | `GET /grub/boot.env HTTP/1.1` (uses up the one-shot choice). `GET /grub/preview.env` shows the same answer without using it up. |
| Response | `200 OK`, `Content-Type: text/plain`, `Content-Length: 1024`, `Cache-Control: no-store`, `Connection: close`, then the env block |
| Values | `wake_boot=0` → Linux, `wake_boot=1` → Windows |
| Range | `Range: bytes=N-` → `206 Partial Content` (GRUB reopens with a Range after seeking back) |
| Errors | `404` other paths, `405` non-GET, `400` malformed, `431` headers over 4 KiB, `403` source IP not in `GRUB_ALLOWED_IPS` |
| Server timeouts | 5 s to receive the request; at most 32 connections at once |
| GRUB timeouts | TCP connect about 16 s (a whole boot with Wake stopped took 19 s in QEMU); ARP for a host that's off about 16 s; DHCP with no DHCP server can take tens of seconds |
| Failure | Any failure leaves `wake_boot=0`: GRUB boots Linux after its normal menu timeout |

## 3. Requirements

- **The dual-boot PC:**
  - Arch Linux + Windows, booting x86_64 **UEFI** GRUB 2. Written against GRUB 2.14; the `(http,IP:PORT)` port syntax needs a recent GRUB.
  - A wired Ethernet NIC that supports Wake-on-LAN.
  - In the firmware setup:
    - **UEFI network stack enabled.** The option is usually called "Network Stack", "IPv4 PXE Support" or "UEFI Network". Without it GRUB sees no network card; the PC still boots Linux, but Wake can't choose.
    - **Network boot may also need to be in the boot order**, *after* your disk. Much firmware only starts drivers for devices in the boot list. The QEMU test shows exactly this: with the NIC out of the boot order, GRUB reports `no network card found`; with it listed after the disk, everything works. The disk still boots first, so this doesn't slow anything down.
    - Wake-on-LAN / "Power On by PCI-E" enabled, and ErP / deep sleep disabled.
- **The server:** any always-on Linux machine on the **same LAN** with Docker (Portainer optional).
- **Building without Docker:** Rust 1.85+ (`rustup`).

## 4. Building

```bash
git clone <this repo> wake && cd wake
docker build -t wake:latest .
```

It's a multi-stage build: `rust:1-alpine` compiles a static musl binary, and the runtime is plain `alpine:3` with that binary. Everything the page needs (HTML, CSS, JS, fonts, icons) is compiled into the binary.

To build and run without Docker:

```bash
cargo build --release
PC_MAC=aa:bb:cc:dd:ee:ff PC_IP=192.168.1.50 WOL_BROADCAST=192.168.1.255 DATA_DIR=./data ./target/release/wake
```

## 5. Portainer deployment

**Option A: from this Git repository (Portainer builds the image).**

1. Portainer → **Stacks** → **Add stack** → **Repository**.
2. Repository URL: your copy of this repo. Compose path: `docker-compose.yml`.
3. Under **Environment variables**, add at least `PC_MAC` and preferably `PC_IP`, `WOL_BROADCAST` and `WAKE_TOKEN` (see [Configuration](#6-configuration)). Use **Load variables from .env file** with `.env.example` as a starting point.
4. **Deploy the stack.**

**Option B: build on the Docker host, then use the web editor.**

```bash
docker build -t wake:latest .
```

Then Portainer → Stacks → Add stack → **Web editor**, paste `docker-compose.yml`, delete the `build: .` line, add the environment variables and deploy.

**What the stack uses:**

| | |
|---|---|
| Network | `network_mode: host` (required; see below) |
| Ports | `8080/tcp` web and API, `8081/tcp` GRUB. With host networking these are opened directly on the host; `ports:` would be ignored. |
| Volume | `wake-data:/data` holds `state.json`. Deleting it only forgets the last boot, the pending choice and events. |
| Capabilities | none added; keep Docker's default `NET_RAW` for ICMP probes |
| User | runs as uid 10001, not root |
| Health check | `GET /healthz` every 30 s |

**Why host networking?** Wake-on-LAN is a *broadcast*. On Docker's default bridge network, a broadcast from the container only reaches `docker0` and never your LAN, and routers don't forward directed broadcasts from a NATed bridge. Host networking puts the packet on the real LAN. It also means the GRUB port sees the PC's real IP address, which the allowlist and the repeat-request logic depend on. A `macvlan` network also works if you'd rather give Wake its own LAN IP, but host networking is simpler.

With `docker compose` instead of Portainer:

```bash
cp .env.example .env   # then edit it
docker compose up -d --build
docker compose logs -f wake
```

## 6. Configuration

All settings are environment variables. Empty values count as unset.

| Variable | Default | Meaning |
|---|---|---|
| `PC_NAME` | `My PC` | Name shown in the UI |
| `PC_MAC` | *required* | MAC of the PC's wired NIC (`aa:bb:cc:dd:ee:ff`, `aa-bb-…` or `aabbccddeeff`). Several are allowed, comma-separated, for example when Linux and Windows or the BIOS report different ones; each gets its own packets. |
| `PC_IP` | *(none)* | The PC's IP. Enables status checks and, by default, restricts the GRUB port to this address. |
| `WOL_BROADCAST` | `255.255.255.255` | Where magic packets go. Prefer the subnet broadcast, e.g. `192.168.1.255`. |
| `WOL_PORT` | `9` | UDP port for magic packets (7 and 9 are conventional) |
| `WEB_PORT` | `8080` | Web UI and API |
| `GRUB_PROTOCOL_PORT` | `8081` | The GRUB responder |
| `BIND_ADDR` | `0.0.0.0` | Listen address for both ports |
| `DEFAULT_BOOT` | `linux` | What GRUB is told when nothing is chosen (`linux` or `windows`). GRUB's own fallback when Wake is unreachable is always Linux. |
| `BOOT_CHOICE_TTL` | `6h` | An unused choice expires after this. `0` means never. |
| `GRUB_REPEAT_WINDOW` | `60s` | Repeat requests from the same IP within this window get the same answer (max 10 min) |
| `GRUB_ALLOWED_IPS` | `PC_IP` | Comma-separated IPs allowed to ask, or `any`. Empty with no `PC_IP` means any. |
| `PROBE` | `icmp,tcp:22,tcp:3389,tcp:445` | How to tell the PC is on. Any answer counts, and a *refused* TCP connection counts too, because the host answered. |
| `PROBE_INTERVAL` | `5s` | How often to check (every 2 s while waking) |
| `WAKE_TIMEOUT` | `3m` | How long "Waking" or "Booting" lasts before Wake gives up and says so |
| `WAKE_TOKEN` | *(none)* | Optional shared secret for the API and page |
| `DATA_DIR` | `/data` | Where `state.json` lives |
| `RUST_LOG` | `info` | Log level, e.g. `info,wake=debug` |

Durations accept `90s`, `5m`, `6h`, `1h 30m`. An invalid value stops Wake at startup with a message that names the variable.

## 7. Finding the PC's MAC address

On the PC, in Linux:

```bash
ip -br link
```

```bash
cat /sys/class/net/enp6s0/address
```

Use the **wired** interface (`enp…`/`eno…`/`eth…`), not Wi-Fi. In Windows, `getmac /v` in a terminal shows the same address as "Physical Address".

If Linux, Windows, the BIOS or your router's client list disagree, put all of them in `PC_MAC`, separated by commas. Extra packets are harmless:

```text
PC_MAC=b4:7e:00:99:f8:ac,b4:2e:99:f0:f8:ac
``` Give the PC a **DHCP reservation** on your router so `PC_IP` stays the same.

## 8. Testing Wake-on-LAN

**On the PC (Linux), check that WOL is armed:**

```bash
sudo ethtool enp6s0 | grep Wake-on
```

`Wake-on: g` means magic packets are enabled. If it says `d`, turn it on:

```bash
sudo ethtool -s enp6s0 wol g
```

Make it stick across reboots with NetworkManager:

```bash
nmcli connection modify "$(nmcli -g GENERAL.CONNECTION device show enp6s0)" 802-3-ethernet.wake-on-lan magic
```

Or with systemd-networkd, a `.link` file with `WakeOnLan=magic`.

**In Windows:** Device Manager → your Ethernet adapter → Properties.
- On the Advanced tab, enable **Wake on Magic Packet**.
- On the Power Management tab, tick **Allow this device to wake the computer**.
- Turn off **Fast Startup** (Control Panel → Power Options → "Choose what the power buttons do"). Fast Startup often leaves the NIC unarmed.

**On the Docker host, watch the packet leave:**

```bash
sudo tcpdump -i any -n udp port 9
```

```bash
curl -X POST http://SERVER:8080/api/wake
```

You should see three 102-byte UDP packets to your broadcast address. Shut the PC down and try again; it should power on.

## 9. Installing the GRUB integration

Run this on the PC, from Arch Linux. The installer shows you everything and asks before changing anything.

```bash
sudo ./grub/install.sh
```

It:

1. Detects your Linux entry (a `gnulinux-…` id) and your Windows entry (the one that chainloads `bootmgfw.efi`) from `/boot/grub/grub.cfg`, and asks for the Wake server's IP and port.
2. Writes `/etc/default/wake` (never overwrites an existing one).
3. Installs `/etc/grub.d/06_wake` and runs its output through `grub-script-check`.
4. Backs up `grub.cfg` to `grub.cfg.before-wake` and runs `grub-mkconfig -o /boot/grub/grub.cfg`.

Because it's a `/etc/grub.d` generator that no package owns, it survives `grub-mkconfig` and `pacman -Syu`. You never edit `grub.cfg` by hand. To change a setting later:

```bash
sudoedit /etc/default/wake
```

```bash
sudo grub-mkconfig -o /boot/grub/grub.cfg
```

If `/etc/default/wake` is invalid, `06_wake` stops `grub-mkconfig` with a message, and your existing `grub.cfg` is left untouched.

To undo it:

```bash
sudo ./grub/uninstall.sh
```

**What ends up in `grub.cfg`** (DHCP mode):

```text
set wake_boot=0
set default="gnulinux-linux-advanced-…"          # Linux, explicitly
if [ "${grub_platform}" = "efi" ]; then
  set wake_ask=1
  if [ "${wake_ask}" = "1" ]; then
    insmod efinet
    insmod http
    insmod loadenv
    echo "Wake: asking 192.168.1.10:8081 for the next boot..."
    set wake_net=0
    if net_dhcp; then set wake_net=1; fi
    if [ "${wake_net}" = "1" ]; then
      load_env --skip-sig --file (http,192.168.1.10:8081)/grub/boot.env wake_boot
    fi
  fi
fi
if [ "${wake_boot}" = "1" ]; then
  set default="Windows Boot Manager"
  echo "Wake: booting Windows this time."
else
  set wake_boot=0
fi
```

**Settings in `/etc/default/wake`:**

- `WAKE_NET=static` with `WAKE_STATIC_IP=…` skips DHCP. That saves about 1–3 s on *every* boot, and avoids a long wait if the router is down.
- `WAKE_NET_CARD=efinet0` pins one card if the PC has several.
- `WAKE_ONLY_ON_LAN_WAKE=yes` makes GRUB ask only when the firmware reports a LAN wake (SMBIOS wake-up type 6), so power-button boots skip the network entirely. Some firmware misreports this, so test it first.
- Wake always sets GRUB's `default`, so it overrides `grub-reboot`.

## 10. Required GRUB modules

All of these ship with Arch's `grub` package in `/usr/lib/grub/x86_64-efi/`, and `grub-install` copies them to `/boot/grub/x86_64-efi/`:

| Module | Why |
|---|---|
| `efinet` | the NIC, through the firmware's UEFI SNP driver |
| `net` | IP, ARP, DHCP, `net_dhcp` / `net_add_addr` (loaded by efinet/http) |
| `http` | the `(http,SERVER:PORT)` device |
| `loadenv` | `load_env` with a whitelist |
| `smbios` | only with `WAKE_ONLY_ON_LAN_WAKE=yes` |

Check they're installed:

```bash
ls /boot/grub/x86_64-efi/{efinet,net,http,loadenv}.mod
```

With **Secure Boot**, GRUB may refuse to load modules that aren't built into its signed image. Wake was designed with Secure Boot off; if you turn it on, build these modules into your signed GRUB image.

## 11. Testing a Linux boot

1. Check what GRUB would get, without using the choice up:
   ```bash
   curl -s http://SERVER:8081/grub/preview.env | head -2
   ```
2. With nothing chosen, reboot the PC. You should see `Wake: asking … for the next boot...` briefly, then Linux. The web page's "Last boot" shows `Linux`, and the event log says "Nothing was waiting".
3. **Fallback test:** stop the container (`docker stop wake`) and reboot. GRUB waits up to about 16 s, then boots Linux. Start the container again.
4. **No-network test:** unplug the cable and reboot. GRUB boots Linux after DHCP gives up.

**By hand in the GRUB shell:** press `c` at the menu, then:

```text
insmod efinet
net_ls_cards
net_dhcp
net_ls_addr
insmod http
insmod loadenv
load_env --skip-sig --file (http,192.168.1.10:8081)/grub/preview.env wake_boot
echo $wake_boot
```

## 12. Testing a Windows boot

```bash
curl -X POST http://SERVER:8080/api/boot/windows
```

Then reboot the PC (or shut it down and `curl -X POST http://SERVER:8080/api/wake`). GRUB prints `Wake: booting Windows this time.`, the menu highlights Windows, and it boots when the menu timeout ends. The page shows **Booting**, then **Awake**, and "Next boot" returns to Linux.

Reboot Windows normally and you'll land in Linux: the choice was one-shot.

## 13. One-shot boot behaviour

- **Picking an OS** stores `next_boot = {os, set_at, expires_at}` and writes it to disk right away.
- **The first `GET /grub/boot.env` uses it up.** Under a single lock, Wake takes the choice (or the default), records `last_boot = {os, time, ip}`, and **saves that to disk before replying**. A crash can't hand out the same choice twice.
- **Repeats inside one boot get the same answer.** GRUB's network file layer reopens the connection when a reader seeks backwards, and `load_env`'s file open does exactly that, so one boot can send several GETs, some with `Range:`. Requests from the **same IP** within `GRUB_REPEAT_WINDOW` (60 s) get the same OS. A request from another IP after the choice is used gets the default.
- **The next real boot gets Linux.** A reboot takes much longer than 60 s from GRUB back to GRUB. That includes Windows Update restarts, which land in Linux: that's the one-shot rule working, not a bug. Pick Windows again if an update needs several restarts.
- **Expiry.** An unused choice expires after `BOOT_CHOICE_TTL` (6 h), so a forgotten Windows pick doesn't surprise you next week.
- **Restarts.** The choice, the last answer and the repeat window all survive a Wake or Docker restart (`/data/state.json`, written atomically). A corrupt state file is moved to `state.json.corrupt-<time>`, and Wake starts fresh with Linux and logs a warning in the UI.
- **Concurrency.** Simultaneous requests are serialised by the lock. The test suite fires 100 concurrent GRUB requests from different IPs, and exactly one gets Windows.

## 14. Troubleshooting Wake-on-LAN

- **The packet never shows up in `tcpdump`.** Check the Wake log (`docker logs wake`) for `sent magic packet` and its target. Make sure the stack uses `network_mode: host`.
- **The packet shows up but the PC stays off:**
  - Check the BIOS WOL setting and ErP.
  - Check `ethtool … | grep Wake-on` shows `g` **before** shutting down.
  - Turn off Windows Fast Startup.
  - Use a wired connection (Wi-Fi WOL is rare).
  - Some NICs wake only from S5 if "Power On by PCI-E" is enabled.
- **It only works after Windows shutdowns (or only after Linux).** Each OS arms the NIC separately; configure both (§8).
- **Wrong interface on a multi-homed host.** Set `WOL_BROADCAST` to that subnet's broadcast (e.g. `192.168.1.255`) instead of `255.255.255.255`.
- **The PC is on another VLAN or subnet.** Broadcasts don't cross routers. Run Wake on a host in the PC's subnet.

## 15. Troubleshooting Docker networking

- **`ports:` has no effect.** That's expected with host networking. Wake listens on the host's `WEB_PORT` and `GRUB_PROTOCOL_PORT` directly. Check with:
  ```bash
  ss -tlnp | grep -E ':8080|:8081'
  ```
- **"can't listen on WEB_PORT".** Something else on the host uses that port. Change `WEB_PORT` or `GRUB_PROTOCOL_PORT`.
- **The GRUB port answers `403`.** The request came from an IP other than `PC_IP`. Check the log line `GRUB request from an address not in GRUB_ALLOWED_IPS` for the real source, fix `PC_IP` or the DHCP reservation, or set `GRUB_ALLOWED_IPS`.
- **The host firewall blocks the PC.** Allow TCP 8081 (and 8080 for your phone) from the LAN, e.g. with ufw:
  ```bash
  sudo ufw allow from 192.168.1.0/24 to any port 8080,8081 proto tcp
  ```
- **Status is always "Offline" but the PC is on.** Windows blocks ping by default. Add a port that answers (`tcp:3389` with RDP on, `tcp:445` with file sharing) or allow ICMP echo in Windows Firewall. Test from the host:
  ```bash
  nc -vz 192.168.1.50 3389
  ```
- **The container won't start with "operation not permitted".** The binary carries `cap_net_raw` as a file capability, so a stack that drops `NET_RAW` can't run it. Remove the `cap_drop`.
- **"state can't be saved".** The volume isn't writable by uid 10001, which usually happens with a bind mount. Fix it with:
  ```bash
  sudo chown -R 10001:10001 /path/to/wake-data
  ```

## 16. Troubleshooting GRUB networking

Press `c` at the GRUB menu and try the commands in §11 step by step.

| Symptom in GRUB | Cause and fix |
|---|---|
| `net_ls_cards` prints nothing, or `no network card found` | The firmware didn't start its UEFI network driver. Enable "Network Stack" / "IPv4 PXE Support", put network (PXE) boot in the boot order **after** the disk, and try with "Fast Boot" off. |
| `net_dhcp` hangs, then fails | No DHCP answer, or the wrong card. Use `WAKE_NET_CARD=efinet0` or `WAKE_NET=static`. |
| `error: couldn't resolve hardware address` / time-out | The Wake host is off, or on another subnet without a gateway. For static mode across subnets, set `WAKE_STATIC_GW`. |
| `error: connection refused` / `time out opening` | The container isn't running, the port is wrong, or the host firewall blocks it |
| `error: file '/grub/boot.env' not found` | Wake answered 404 or 403. Check the path and `GRUB_ALLOWED_IPS`. |
| `error: invalid environment block` | Something other than Wake answered on that port (a proxy?) |
| Boot is slower | DHCP runs on every boot (about 1–3 s). Use static mode, or `WAKE_ONLY_ON_LAN_WAKE=yes`. |
| Windows entry not found | `WAKE_WINDOWS_ENTRY` must match a menu entry id or title exactly. List them with: `grep -E "^\s*menuentry" /boot/grub/grub.cfg` |

After `grub-mkconfig`, check that the block is in place:

```bash
sudo sed -n '/### Wake/,/### end Wake/p' /boot/grub/grub.cfg
```

## 17. Security considerations

- **Don't expose Wake to the internet.** It is meant for a trusted home LAN. Don't port-forward 8080 or 8081. Use a VPN (WireGuard, Tailscale) to reach it from outside.
- **Set `WAKE_TOKEN`** if anyone else uses your network. With it set, every `/api/*` call needs `Authorization: Bearer <token>`, and the page asks for the token once per device (stored as an HttpOnly, SameSite=Strict cookie). Without it, anyone on the LAN can wake the PC and pick its OS.
- **Cross-site requests are refused** even without a token. A browser POST whose `Origin` doesn't match the page, or that has `Sec-Fetch-Site: cross-site`, gets a 403, so a malicious website can't make your phone's browser wake the PC. `curl` sends no `Origin` and keeps working.
- **The GRUB port is minimal.** It serves two read-only paths, allows only `PC_IP` by default, caps connections and times out slow clients. GRUB's HTTP has no TLS, so someone on your LAN could forge a reply. Because of the `load_env` whitelist, the worst a forged reply can do is choose Linux or Windows. It can't run commands in GRUB.
- The container runs as a non-root user with one file capability (`cap_net_raw`, for ping). The page sends a strict Content-Security-Policy and loads nothing from other hosts. Fonts are bundled.

---

## API

Every response is JSON with `Cache-Control: no-store`. With a token, add `-H "Authorization: Bearer $WAKE_TOKEN"`.

| Method | Path | Does |
|---|---|---|
| `GET` | `/api/status` | Everything the page shows |
| `POST` | `/api/boot/linux` · `/api/boot/windows` | Set the one-shot choice |
| `POST` | `/api/boot/default` | Clear the choice |
| `POST` | `/api/wake` | Send the magic packet (keeps the current choice) |
| `POST` | `/api/wake/linux` · `/api/wake/windows` | Set the choice, then wake |
| `GET` | `/api/events` | Server-Sent Events: a `status` event on every change |
| `POST` / `DELETE` | `/api/session` | Exchange the token for a browser cookie, or forget it |
| `GET` | `/healthz` | `ok` (never needs a token) |

```bash
curl http://SERVER:8080/api/status
```

```bash
curl -X POST http://SERVER:8080/api/wake/windows
```

```bash
curl -X POST http://SERVER:8080/api/wake/linux
```

```bash
curl -X POST -H "Authorization: Bearer $WAKE_TOKEN" http://SERVER:8080/api/boot/windows
```

`POST /api/wake/windows` returns:

```json
{ "ok": true, "already_online": false, "next_boot": "windows",
  "message": "Magic packet sent. My PC should start Windows.", "status": { "...": "..." } }
```

Errors look like `{"error": "bad_request", "message": "'macos' isn't an OS Wake knows. Use linux or windows."}`:

| Status | Meaning |
|---|---|
| 400 | Bad OS name or body |
| 401 | Token needed |
| 403 | Cross-site request |
| 502 | The magic packet couldn't be sent. The choice is still saved. |

If the PC is already on, `/api/wake` still returns 200, sets `"already_online": true`, and keeps the choice for its next restart.

## Tests

```bash
cargo test
```

- **Unit tests** (`src/*`):
  - the one-shot consume, TTL expiry and the repeat window;
  - 100 concurrent requests;
  - persistence and corrupt-file recovery;
  - MAC parsing and the 102-byte packet, including a real UDP send;
  - the env block layout;
  - request parsing and Range handling;
  - status rules;
  - every configuration error.
- **Integration tests** (`tests/`):
  - `api.rs`: every endpoint in-process, including the token, cookie and cross-site guards, and the locked page;
  - `grub_protocol.rs`: the GRUB port over real TCP, byte for byte, including status codes, the allowlist and the slow-client timeout;
  - `flow.rs`: phone → wake → GRUB → back to default, and a restart between wake and GRUB.
- **GRUB script check:** render the generator with a sample config and validate it:
  ```bash
  WAKE_CONFIG=grub/wake.default sh grub/06_wake | grub-script-check
  ```
- **Real GRUB in QEMU:** `tests/qemu/grub-boot-test.sh` builds a standalone GRUB 2.14 EFI image. The image contains the output of `06_wake` and two test entries that print which one booted and power off. It boots that image under OVMF with a virtio NIC and user-mode networking, against a scratch Wake instance on the host. It needs `qemu-system-x86_64`, `edk2-ovmf` and `grub`, and never touches the host's GRUB.
  ```bash
  tests/qemu/grub-boot-test.sh
  ```
  | Scenario | Expected | Result (GRUB 2.14, OVMF 202608) |
  |---|---|---|
  | Windows chosen | Windows, choice used up | pass, 4 s |
  | Nothing chosen / Linux chosen | Linux | pass, 4 s |
  | Static address instead of DHCP | Windows | pass, 3 s |
  | No network card | Linux | pass, 3 s |
  | Server replies `1`, `wake_boot=banana`, or a block that sets `default=` | Linux (`load_env` whitelist) | pass |
  | Wake not running | Linux | pass, 19 s (GRUB's ~16 s connect timeout) |

  In these runs GRUB made one request per boot. The repeat window is there in case other firmware or GRUB builds reopen the file.
- **Manual GRUB tests**, which need your real firmware: §11 and §12.

## Project layout

```text
src/main.rs      startup, listeners, graceful shutdown
src/config.rs    environment variables → validated Config
src/state.rs     boot choice, one-shot rules, events (one async mutex)
src/store.rs     atomic JSON persistence + corrupt-file recovery
src/wol.rs       MAC parsing, magic packets, UDP broadcast
src/probe.rs     icmp/tcp probes, PC status, status monitor
src/grub.rs      GRUB env block + minimal HTTP responder
src/api.rs       REST API, SSE, token and cross-site guards
src/web.rs       page, static assets, security headers
templates/       index.html (Askama)
static/          Broadsheet tokens + Wake styles, app.js, fonts, icons, manifest
grub/            06_wake generator, /etc/default/wake template, install/uninstall
```

The page uses the **Broadsheet** design system: its colour tokens (the Morning and Late editions follow your phone's light or dark mode), Newsreader / Instrument Sans / IBM Plex Mono, hairline rules, and the stitched primary button. The motion is springy but respects `prefers-reduced-motion`. The fonts are bundled under the SIL Open Font License.
