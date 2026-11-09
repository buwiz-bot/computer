# Deploy buwiz-computer (use the box)

This is the plan for **actually running** a buwiz-computer guest and talking to it from a laptop. It matches this repo as of the `docker-compose.yml` / `docker/Dockerfile` in tree. It does **not** provision anything for you.

**Repo of record:** [https://github.com/buwiz-bot/computer](https://github.com/buwiz-bot/computer) (`main`).

**First test (do this):** one **Akamai Cloud** VM (this is **Linode**, not Vultr), **guest Compose only**, **private access** (SSH tunnel or Tailscale). Drive it with the `buwiz-computer` CLI / SDKs from your laptop. Do **not** open ports **1337 / 1340 / 6901-6905** on the public internet.

Vultr is a different company and a different API. Tokens, Terraform providers, and regions do not transfer.

---

## A. What you are deploying

The **product** is:

- the guest Docker image (`buwiz-computer:local` in Compose; tagged releases push `ghcr.io/buwiz-bot/computer`)
- `box-exec` + `box-host` inside that image
- the connect-only `buwiz-computer` CLI and TypeScript / Python / Rust SDKs

You start the container. Then you call `connect(execUrl, hostUrl, token)`. There is **no** `Sandbox.create()` / docker-run helper in the SDKs.


| Path | What runs on the VM | Who talks to the guest |
| --- | --- | --- |
| **1 — recommended** | Guest only (`docker compose up`) | Your laptop: CLI / SDK |

Production consumers are CLI/SDK against the guest HTTP wire (`:1337` exec, `:1340` host).

### What the guest actually is

One container, not privileged. It starts as root for `box-keeper`, which starts everything else as a user of its own ([`USERS.md`](USERS.md)). Compose drops all capabilities but the few that takes, and sets `no-new-privileges` plus pids/memory limits. **That is not a kernel sandbox.** **No** `docker.sock`. Chromium is started with `--no-sandbox` because it is not root.

Inside (see [`docs/ARCHITECTURE.md`](ARCHITECTURE.md)):

- `box-exec` on **1337** (exec, files, CUA)
- `box-host` on **1340** (health, ready, info, desktop/chrome)
- Xvfb **1280×800**, xfce, voxl **6901** (and **6902–6905** for the Bots' own screens)
- Chromium, with no DevTools port

Compose publishes **1337, 1340, 6901–6905** as `127.0.0.1:1337:1337` (loopback on the VM). Reach them with an SSH tunnel, Tailscale, or similar. Healthcheck: `curl -fsS` on the public `/v1/health` of 1340 and 1337, with no token, so it never puts one in argv (start period 25s, 5s interval, 12 retries). `/v1/ready` is for callers. `shm_size` is **256mb**. Bind mounts: `./workspace-data:/workspace`, `./chrome-profile:/home/box/desktops/main/chrome-profile`.

`BOX_CORS_ORIGINS` is an explicit allowlist (`*` is ignored). Compose does **not** currently pass that env into the container; laptop CLI/SDK does not need CORS. Leave it unset unless a **browser** will call the guest directly (unusual).

---

## B. What we need from you (checklist)

Reply with values. **Do not paste an Akamai API token into chat.** Keep it in a password manager, or in your shell as `LINODE_TOKEN` when **you** run OpenTofu later.

### Akamai Cloud (Linode) — first test

Create the token yourself:

1. Log in to [Cloud Manager](https://cloud.linode.com/).
2. Username (top right) → **API Tokens** ([direct](https://cloud.linode.com/profile/tokens)).
3. **Create a Personal Access Token**. Label it e.g. `buwiz-computer-test`. Expiry: short (a week) is enough for a test.
4. Scopes: Read/Write on **Linodes**, **Firewalls** (Cloud Firewalls often need `firewall:read_write` plus account access — if the UI is unclear, “Select All Read/Write” for a short-lived test token is acceptable). You do **not** need Object Storage / LKE for path 1.
5. Copy it once; Cloud Manager will not show it again. Never commit it.

OpenTofu / Terraform reads **`LINODE_TOKEN`** (Linode APIv4). Do not put the token in `.tf` / `.tfvars`.

```bash
export LINODE_TOKEN="…"   # your machine only
```

| # | Item | Your answer |
| --- | --- | --- |
| 1 | Akamai API token | Created (yes/no). **Kept local** — you run tofu, or we use a secret store later. |
| 2 | Region | Pick one. Closest defaults from the Philippines: **`ap-south`** (Singapore) or **`sg-sin-2`** (Singapore 2), then **`jp-tyo-3`** / **`ap-northeast`** (Tokyo). US/EU only if you want that latency on purpose (`us-ord` Chicago, `us-east` Newark, `eu-west` London). Confirm the slug in Cloud Manager before create. |
| 3 | SSH public key | Paste the **public** key (`ssh-ed25519 …` or `ssh-rsa …`). |
| 4 | Shape | **Guest-only (path 1)** or **guest + demo UIs (path 2)** |
| 5 | DNS | Domain name **or** “IP only for the test” (IP-only is right for the first private test) |
| 6 | `BOX_TOKEN` | Generate a strong one for you **or** you supply one. Never use `dev-box-token` on a VM. |
| 7 | CORS origins | Almost always **none**. Only if a browser will call the guest. |
| 8 | Size / budget | Confirm the size in [§C](#c-instance--machine-sizing) (recommended: **Linode 8 GB Shared**, ~**$48/mo** / **$0.072/hr**, delete when done). |

Optional later: a domain + DNS at Akamai or elsewhere (only for public HTTPS). Tailscale account if you prefer overlay VPN over SSH tunnels.

---

## C. Instance / machine sizing

This guest is **RAM-heavy**: Debian + Xvfb + xfce + voxl + Chromium + two Rust daemons. The **first** deploy also **builds the image on the VM** (Rust `1.99` builder compiles `box-exec` / `box-host`, then a `debian:bookworm-slim` runtime with Chromium). `.github/workflows/publish-image.yml` can push GHCR on `v*` tags; until you pull that, build on the VM.

| Plan | Type slug | vCPU | RAM | Disk | ~price (core regions, 2026 list — confirm in Cloud Manager) | Verdict |
| --- | --- | --- | --- | --- | --- | --- |
| Nanode 1 GB | `g6-nanode-1` | 1 | 1 GB | 25 GB | ~$5/mo | No. |
| Linode 2 GB | `g6-standard-1` | 1 | 2 GB | 50 GB | ~$12/mo | No. Chromium will thrash. |
| Linode 4 GB | `g6-standard-2` | 2 | 4 GB | 80 GB | ~$24/mo | Runtime **maybe** after a prebuilt image. **First `docker compose up --build` will likely OOM.** |
| **Linode 8 GB Shared** | **`g6-standard-4`** | **4** | **8 GB** | **160 GB** | **~$48/mo, ~$0.072/hr** | **Yes — first Akamai test.** |
| Dedicated 8 GB | `g6-dedicated-4` | 4 | 8 GB | 160 GB | ~$72/mo | If you want no noisy neighbor. Same RAM. Not required for a Saturday test. |

**Image slug:** `linode/ubuntu24.04` (Ubuntu 24.04 LTS).

**Swap is a last resort, not a plan.** Do not size a 4 GB box and “add 8 GB swap” so Chromium and `cargo` can page. If the build OOMs on 8 GB (unlucky), resize up to **Linode 16 GB** (`g6-standard-6`) for the build, then you can resize down for idle runtime.

**Disk:** 160 GB is plenty: Docker builder layers (Rust image is large), Chromium in the runtime, Compose volumes, a bit of workspace. The bind mounts are your data; the image layers are disposable.

**Runtime-only later** (pull a prebuilt image, no compiler): **4 GB** can work for one desktop guest. Still skip 2 GB.

**Exec/files only** (`BOX_DESKTOP=0`, `BOX_DESKTOP_REQUIRED=0`, no Chromium): RAM drops a lot. Still do not use a Nanode for the first build.

---

## D. Networking and TLS (critical)

### Ports (verify against Compose)

| Port | Publish? | Role |
| --- | --- | --- |
| **1337** | Compose yes (`127.0.0.1`) | `box-exec` — Bearer `BOX_TOKEN` except `GET /v1/health` |
| **1340** | Compose yes (`127.0.0.1`) | `box-host` — Bearer except `GET /v1/health`. **`GET /v1/ready` requires Bearer.** |
| **6901–6905** | Compose yes (`127.0.0.1`) | voxl — **not** Bearer; its own password is `BOX_VIEWER_PASSWORD` |
| **8790** | Compose yes (`127.0.0.1`) | `box-egress-tunnel` WS (off unless `BOX_EGRESS_TUNNEL=1`). See [EGRESS.md](EGRESS.md). |
| 8791 | **no** | Chromium CONNECT proxy |
| 8792 | **no** | tunnel admin (box-host probe) |

`BOX_TOKEN` is bearer auth. It is **not** a substitute for TLS. Do **not** expose exec/host as raw HTTP on a public IP. Compose reads the token and the viewer password from `./secrets/box_token` and `./secrets/box_viewer_password`; run `bash scripts/write-secrets.sh` (it reads `BOX_TOKEN` / `BOX_VIEWER_PASSWORD` from the environment or `.env`) before `docker compose up`, or Compose fails on the missing secret file.

Health is **unauthenticated** and returns only `{"status":"ok"}`. Ready requires Bearer.

voxl is a **full desktop session** (keyboard/mouse on 1280×800). Treat **6901–6905** like a KVM. The viewer password is **independent of `BOX_TOKEN`**. Keep these ports off the public internet; loopback publish is the default.

### Two network modes

**1. Private (first test — do this)**

- One VM. Cloud Firewall **default-deny** inbound. Allow **TCP 22** only (optionally from your home IP `/32`). IPv6: same policy (`DROP` unless you allow SSH on v6).
- Do **not** allow 1337, 1340, 6901–6905 on the public firewall.
- Reach the guest from the laptop by **either**:
  - **SSH tunnel** (simplest, no extra accounts):

    ```bash
    ssh -N \
      -L 1337:127.0.0.1:1337 \
      -L 1340:127.0.0.1:1340 \
      -L 6901:127.0.0.1:6901 \
      -L 8790:127.0.0.1:8790 \
      root@VM_IPV4
    ```

    CLI uses `http://127.0.0.1:1337` and `http://127.0.0.1:1340`.
  - **Tailscale** (or WireGuard): install on the VM + laptop. Point the CLI at `http://<tailscale-100.x>:1337`. Cloud Firewall still blocks the **public** NIC. Tailscale may fall back to DERP if UDP **41641** is not open — that is fine for a test.

Compose publishes `127.0.0.1`. **Firewall is still wise** even for a “private” test: Akamai gives the VM a **public IPv4** (and usually IPv6). Loopback publish is now the default in this repo:

```yaml
# docker-compose.yml (this tree)
services:
  box:
    ports:
      - "127.0.0.1:1337:1337"
      - "127.0.0.1:1340:1340"
      - "127.0.0.1:6901-6905:6901-6905"
      - "127.0.0.1:8790:8790"
```

Loopback bind + SSH tunnel is the tightest combo. Loopback bind **breaks** “dial the Tailscale IP :1337” (Tailscale is not `127.0.0.1`); use the tunnel over Tailscale SSH instead.

**2. Public HTTPS (only after the private test works)**

- Domain → **Caddy** or nginx on the VM → `127.0.0.1:1337` / `:1340` / (optional) `:6901`.
- Let’s Encrypt on **443**. Firewall: **22 + 443** only. Still send `Authorization: Bearer`.
- Subdomains are simpler than path prefixes (the client appends `/v1/...` to the base URL):

```caddy
exec.example.com {
  reverse_proxy 127.0.0.1:1337
}
host.example.com {
  reverse_proxy 127.0.0.1:1340
}
# optional; still sensitive
desktop.example.com {
  reverse_proxy 127.0.0.1:6901
}
```

Then:

```bash
export BUWIZ_COMPUTER_EXEC_URL=https://exec.example.com
export BUWIZ_COMPUTER_HOST_URL=https://host.example.com
export BUWIZ_COMPUTER_TOKEN='…'
```

IP-only HTTP on `:1337` is not an acceptable “public” mode.

### Firewall philosophy

Default-deny. SSH keys, not passwords. Unattended-upgrades on Ubuntu. Cloud Firewalls on Akamai are **free** and sit in front of the VM ([docs](https://techdocs.akamai.com/cloud-computing/docs/cloud-firewall)). Still keep `ufw` or nftables if you want defense in depth. Compose now publishes loopback; a Cloud Firewall is still the right default-deny for SSH.

---

## E. Multiple deployment methods

Simplest → more infra. Same guest, same Compose, same firewall idea everywhere.

Akamai Cloud Compute **is** Linode. The OpenTofu/Terraform provider for **VMs** is **`linode/linode`**, token **`LINODE_TOKEN`**. The **`akamai/akamai`** provider is CDN / Property Manager / edge — **not** this VM. Do not mix them.

### 1. Manual VM + Docker Compose (start here)

**When:** Saturday afternoon, path 1, you want a box you can SSH into and poke.

**Akamai Cloud Manager**

1. Create → Linode. Image **Ubuntu 24.04 LTS** (`linode/ubuntu24.04`). Type **Linode 8 GB** (`g6-standard-4`). Region from the checklist. SSH key, **no** password login if the UI allows key-only (or set a throwaway root password and disable password SSH after).
2. Create a **Cloud Firewall**: inbound **DROP**, allow TCP **22**. Attach it to the Linode. Allow IPv6 SSH only if you will use v6; otherwise drop v6 inbound too.
3. SSH in. Install Docker Engine + Compose plugin (Ubuntu 24.04):

   ```bash
   sudo apt-get update
   sudo apt-get install -y ca-certificates curl git
   curl -fsSL https://get.docker.com | sudo sh
   sudo usermod -aG docker "$USER"
   # log out and back in so docker works without sudo
   ```

4. Clone and run (**builds on the VM** — first time is slow, 10–30+ minutes):

   ```bash
   git clone https://github.com/buwiz-bot/computer.git
   cd computer
   umask 077
   openssl rand -base64 32 > /tmp/box-token
   BOX_VIEWER_PASSWORD=$(openssl rand -base64 24)
   cat > .env <<EOF
   BOX_TOKEN=$(cat /tmp/box-token)
   BOX_VIEWER_PASSWORD=${BOX_VIEWER_PASSWORD}
   BOX_ID=akamai-test
   BOX_DESKTOP=1
   BOX_DESKTOP_REQUIRED=1
   RUST_LOG=info
   EOF
   chmod 600 .env
   shred -u /tmp/box-token
   mkdir -p workspace-data chrome-profile
   sudo chown -R 1000:1000 workspace-data chrome-profile
   bash scripts/write-secrets.sh   # ./secrets/* — Compose mounts these, see §Secrets
   docker compose up --build -d
   docker compose ps
   curl -fsS http://127.0.0.1:1340/v1/health
   ```

   `/v1/ready` takes the token. Check it with the CLI from your laptop ([§F](#f-day-1-use-it-flow)), where it goes in `BUWIZ_COMPUTER_TOKEN`, not in a command line.

   If the GitHub repo is private, use a deploy key or `scp` the tree instead of a plaintext PAT in `git clone`.

5. From the **laptop**, open the SSH tunnel ([§D](#d-networking-and-tls-critical)) and follow [§F](#f-day-1-use-it-flow).

**Pros:** Obvious, debuggable, matches local README. **Cons:** Click-ops, easy to forget the firewall, first build is heavy.

`image: buwiz-computer:local` is the Compose default. Tagged `v*` builds push `ghcr.io/buwiz-bot/computer` for arm64 and amd64. First test without a pull still **builds on the VM**.

### 2. Cloud-init / StackScript / user-data

**When:** you will create/destroy this VM more than once, still without OpenTofu.

Akamai: Metadata **user-data** (cloud-init) on images that support it, **or** a [StackScript](https://techdocs.akamai.com/cloud-computing/docs/stackscripts). Akamai’s own docs now prefer Metadata/cloud-init for new work; StackScripts do not run in **distributed** compute regions. Same script works as AWS user-data, GCP startup-script, Azure custom-data.

**Do not put `BOX_TOKEN` or `LINODE_TOKEN` in user-data.** cloud-init logs are world-readable on the instance.

```yaml
#cloud-config
package_update: true
packages:
  - git
  - ca-certificates
  - curl
ssh_pwauth: false
runcmd:
  - curl -fsSL https://get.docker.com | sh
  - git clone https://github.com/buwiz-bot/computer.git /opt/buwiz-computer
  - mkdir -p /opt/buwiz-computer/workspace-data /opt/buwiz-computer/chrome-profile
  - chown -R 1000:1000 /opt/buwiz-computer/workspace-data /opt/buwiz-computer/chrome-profile
  - |
      echo "Clone done. SSH in, write /opt/buwiz-computer/.env with BOX_TOKEN, then:"
      echo "  cd /opt/buwiz-computer && docker compose up --build -d"
```

Pass this as Metadata `user_data` (API wants **base64**). Then SSH, write `.env`, compose up. Unattended “clone + compose up” is possible if you inject the token via a secret mount later; do not bake it into the StackScript body (visible via API).

**Pros:** Repeatable create. **Cons:** First boot still compiles; secrets handling is easy to get wrong.

### 3. OpenTofu (preferred) or Terraform

**When:** after the manual VM works, so you can recreate/destroy cleanly. **Do not start here** for the first “does Chromium even fit” test.

**OpenTofu** is the OSS default. Terraform is equivalent for this (same HCL). Pin `linode/linode` v3 (`~> 3.0`). Auth: `export LINODE_TOKEN=…` on **your** machine. Local state is OK for a personal test (`terraform.tfstate` has IPs, not the Linode token if you used the env var — still do not commit state). Never put tokens in `*.tfvars` committed to git.

This pass does **not** ship a full module. Resource list for Akamai:

| Resource | Purpose |
| --- | --- |
| `linode_instance` | `image = "linode/ubuntu24.04"`, `type = "g6-standard-4"`, `region = var.region`, `authorized_keys = [var.ssh_public_key]`, `metadata { user_data = base64encode(file("cloud-init.yaml")) }`. Avoid `root_pass` if keys suffice. |
| `linode_firewall` | `inbound_policy = "DROP"`, `outbound_policy = "ACCEPT"`, allow TCP 22 (and 443 only if you later go public). Cover **ipv4 and ipv6**. Attach with `linodes = [linode_instance.box.id]` (or `linode_firewall_device`). |
| `linode_domain` / `linode_domain_record` | Optional, public HTTPS only. |
| Outputs | IPv4, IPv6, `ssh root@…`, suggested `-L` tunnel command. |

Sketch (not a maintained module — check [registry.terraform.io/providers/linode/linode](https://registry.terraform.io/providers/linode/linode/latest/docs) before apply):

```hcl
terraform {
  required_providers {
    linode = {
      source  = "linode/linode"
      version = "~> 3.0"
    }
  }
}

provider "linode" {} # LINODE_TOKEN

resource "linode_instance" "box" {
  label           = "buwiz-computer-test"
  region          = var.region
  type            = "g6-standard-4"
  image           = "linode/ubuntu24.04"
  authorized_keys = [var.ssh_public_key]
  metadata {
    user_data = base64encode(file("${path.module}/cloud-init.yaml"))
  }
}

resource "linode_firewall" "box" {
  label           = "buwiz-computer-test"
  inbound_policy  = "DROP"
  outbound_policy = "ACCEPT"

  inbound {
    label    = "ssh"
    action   = "ACCEPT"
    protocol = "TCP"
    ports    = "22"
    ipv4     = ["0.0.0.0/0"] # better: ["YOUR.IP/32"]
    ipv6     = ["::/0"]
  }

  linodes = [linode_instance.box.id]
}

output "ipv4" { value = linode_instance.box.ip_address }
output "ssh" { value = "ssh root@${linode_instance.box.ip_address}" }
output "tunnel" {
  value = "ssh -N -L 1337:127.0.0.1:1337 -L 1340:127.0.0.1:1340 -L 6901:127.0.0.1:6901 -L 8790:127.0.0.1:8790 root@${linode_instance.box.ip_address}"
}
```

**Other clouds — copy Compose + cloud-init, swap the VM resource:**

| Cloud | VM | Firewall | Image / bootstrap |
| --- | --- | --- | --- |
| **AWS** | One **EC2** `t3.large` (2 vCPU / 8 GB) or `t3.xlarge` if the build is tight. No ALB for the first test. | **Security group**: 22 (your IP), later 443. Not 1337/1340/6901-6905. | Ubuntu 24.04 AMI (Canonical). Same cloud-init as `user_data`. Provider `hashicorp/aws`. |
| **GCP** | One **Compute Engine** `e2-standard-2` (2 vCPU / 8 GB). | VPC firewall **tags**: `allow-ssh`, later `allow-https`. | `ubuntu-2404-lts`. `metadata.startup-script`. Provider `hashicorp/google`. |
| **Azure** | One VM **Standard_D2s_v5** or **Standard_B2ms** (~8 GB). | **NSG**: 22, later 443. | Ubuntu 24.04 LTS. Custom data = cloud-init. Provider `hashicorp/azurerm`. |

Caddy on the VM beats an ALB/HTTPS load balancer for a single box. Shared philosophy: one VM, Docker Compose, default-deny, private first.

**Pros:** Recreate/destroy is a command; cost stops when you `tofu destroy`. **Cons:** Provider quirks, state files, easy to commit secrets if you are sloppy.

### AWS EC2 (broker-managed computers)

A different shape from the VMs above: Buwiz Bot's broker launches one EC2 instance per computer, hibernates it when idle and terminates it when the computer is deleted. [deploy/aws/README.md](../deploy/aws/README.md) is the OpenTofu module for what those instances launch into (launch template, network, SSH-only security group), the broker's IAM policy for your own account (`tofu output -raw broker_policy_json`), the broker's settings, cost, and how to tear it down. OpenTofu never creates an instance there.

No OpenTofu? [deploy/aws/cloudformation/buwiz-computer.yaml](../deploy/aws/cloudformation/buwiz-computer.yaml) makes the same setup from the AWS console, with no access key in it: [deploy/aws/README.md](../deploy/aws/README.md#without-opentofu-cloudformation) has the steps, and `deploy/aws/check-parity.sh` fails if the two drift.

E2B is the other provider the broker runs computers on. [deploy/e2b/README.md](../deploy/e2b/README.md) builds the box template in your own E2B team with `E2B_API_KEY=… bash deploy/e2b/build.sh`, and prints the name to set as `BUWIZ_E2B_TEMPLATE`.

### 4. Container-as-a-service? Kubernetes?

The guest is a **long-running** container with X11, voxl, and Chromium — not a 10ms serverless function.

| Platform | Fit |
| --- | --- |
| Lambda / Cloud Functions | No. |
| Cloud Run / App Service “scale to zero” | Poor. Need min instances, RAM, and **three** HTTP ports (or a sidecar proxy). WebSockets for voxl are awkward. |
| ACI / ECS Fargate / Cloud Run **always-on** | Possible in theory: it is **one** container, not a Compose mesh. You still need ~8 GB, `shm`, persistent disks for `/workspace` + chrome profile, and TLS in front. More glue than a VM. |
| **VM + Compose** | **Do this first.** |
| Kubernetes | Later, if you already have a cluster: one Deployment, Service, two PVCs, `emptyDir` `medium: Memory` ~256Mi for `/dev/shm`. The container starts as root and needs Compose's capability set ([USERS.md](USERS.md)), so no `runAsUser`. Not the first test. |


### 5. Prebuilt image to a registry (second iteration)

Build on a beefy machine or CI, push to **GHCR** (Akamai has **no** first-class managed container registry like ECR — use GHCR, or ECR/GCR/ACR on those clouds). VM only `docker compose pull` / `docker run`.

Until a tag has been pushed, `git clone && docker compose up --build` is the path. The publish workflow is [`.github/workflows/publish-image.yml`](../.github/workflows/publish-image.yml): on `v*` tags it builds `docker/Dockerfile` and pushes `ghcr.io/buwiz-bot/computer`. First publish may need the package set to public in GitHub Packages settings.

---

## F. Day-1 “use it” flow

After the Akamai VM is up and Compose is healthy:

1. **Tunnel** from the laptop (or Tailscale). Confirm:

   ```bash
   curl -fsS http://127.0.0.1:1337/v1/health
   curl -fsS http://127.0.0.1:1340/v1/health
   cargo run -p buwiz-computer -- ready     # token from BUWIZ_COMPUTER_TOKEN, see step 3
   ```

   Ready is **200** when exec is up and (with `BOX_DESKTOP_REQUIRED=1`) Xvfb is up. Chrome is **not** on the ready path — give Chromium a few more seconds before screenshots look interesting.

2. **URLs + token** (private mode):

   - exec: `http://127.0.0.1:1337`
   - host: `http://127.0.0.1:1340`
   - token: `BOX_TOKEN` from the VM `.env` (copy it out over SSH; do not paste it into random chat logs)

   Do **not** use `GET /v1/info` `endpoints` as connect URLs. Those are container-local listen addresses.

3. **CLI from the laptop** (workspace crate; not published to crates.io):

   ```bash
   export BUWIZ_COMPUTER_EXEC_URL=http://127.0.0.1:1337
   export BUWIZ_COMPUTER_HOST_URL=http://127.0.0.1:1340
   export BUWIZ_COMPUTER_TOKEN='…'   # same as BOX_TOKEN

   cargo run -p buwiz-computer -- health
   cargo run -p buwiz-computer -- ready
   cargo run -p buwiz-computer -- exec -- echo ok
   cargo run -p buwiz-computer -- files put hello.txt --content 'hi'
   cargo run -p buwiz-computer -- files get hello.txt
   cargo run -p buwiz-computer -- cua screenshot --png -o /tmp/box.png
   ```

   TypeScript / Python: `BuwizComputer.connect(execUrl, hostUrl, token)` — same three values. No docker helper.

4. **Optional live view:** `http://localhost:6901/` through the tunnel (WebCodecs needs `localhost` or HTTPS). Password = `BOX_VIEWER_PASSWORD`. Not Bearer-authenticated.

**It works when:** `echo ok` returns exit 0, file put/get round-trips, screenshot writes a PNG of the 1280×800 desktop.

---

## G. Security and ops

- **Rotate `BOX_TOKEN`:** the token is read only from `./secrets/box_token`, not from the container environment. Put the new value in `.env`, run `bash scripts/write-secrets.sh`, then `docker compose up -d --force-recreate`. Update the laptop env. Rotate `BOX_VIEWER_PASSWORD` the same way; it is independent of the bearer. If your shell exports the old `BOX_TOKEN`, unset it first; `.env` is read by the script.
- **Do not log tokens.** Do not put them in Compose `docker compose config` pastebins, or CI logs. Compose mounts `./secrets/*` rather than putting values in the container environment, so the token is not in `docker inspect` or in any `/proc/<pid>/environ`. It **is** left at `/run/secrets/box_token` for the life of the box, because the mount is read-only and `box-keeper` cannot unlink it; where the mount does not enforce modes (Docker Desktop's file share) anything in the box, an exec child included, can read it. See README §Auth for what that does and does not protect, and for staging on a writable mount instead. Exec children never get any of these variables.
- SSH **keys**, not passwords. `PermitRootLogin prohibit-password`. `unattended-upgrades`.
- **Disk:** `workspace-data` is **your** files. `chrome-profile` is the main screen's cookies/session. `box-prepare` gives the mounts their owners at start ([USERS.md](USERS.md)); the `chown 1000:1000` above does no harm. Back up the workspace volume if you care; the image is rebuildable.
- **Cost:** an idle VM still bills. Power off or `tofu destroy` / Cloud Manager delete when done. Hourly on 8 GB Shared is on the order of **seven cents**. A forgotten month is **~$48**.
- **CORS:** default is no browser origins. Do not point a random website at 1337.
- **Egress CONNECT unpublished.** Do not add `8791:8791`. WS `8790` is loopback + SSH `-L` only. Prod must not rely on Docker host-network for Facebook/IP geo; see [EGRESS.md](EGRESS.md).

---

## H. Suggested sequence for this week

1. You send the [checklist](#b-what-we-need-from-you-checklist). API token stays on your machine (you run tofu later) or in a secret store — **not chat**.
2. **First deploy:** one Akamai VM, Ubuntu 24.04, 8 GB Shared, Cloud Firewall 22 only, guest Compose only, private access (SSH tunnel).
3. Prove CLI/SDK from the laptop (exec, files, screenshot PNG).
4. Only then: public HTTPS (Caddy + domain).
5. Only then: a small OpenTofu root so create/destroy is boring.
6. Other clouds: same Compose + cloud-init, swap the instance/firewall resources.

---

## I. Open questions / risks

- **Image build time on the VM.** First `docker compose up --build` compiles Rust in `rust:1.99-bookworm` and `rust:1.93-bookworm` (voxl) and apt-installs Chromium. Expect a long first boot. A small VM makes this worse; 8 GB / 4 vCPU is the mitigation. Tagged releases can push `ghcr.io/buwiz-bot/computer` via `.github/workflows/publish-image.yml` (`GITHUB_TOKEN` + `packages: write`). Until you have pulled that image, `git clone && docker compose up --build` is the path.
- **Rust / TS / Python clients speak HTTPS.** Guest daemons still bind HTTP. Put Caddy/nginx in front for TLS. The Rust crate uses hyper-rustls (webpki roots).
- **Chromium RAM.** One desktop guest is the design point. Two guests on one 8 GB VM will hurt.
- **IPv6.** Akamai assigns it. Cloud Firewall must DROP v6 too, or you published the box on v6 while locking v4.
- **Compose publishes loopback.** `127.0.0.1:1337:1337` (and 1340/6901-6905). Tailscale-to-public-NIC still will not hit those ports; use an SSH tunnel (including over Tailscale SSH). Health is open but minimal; voxl is a desktop on loopback.
- **Desktop vs `BOX_DESKTOP=0`.** If you only need exec/files, turn desktop off (`BOX_DESKTOP=0`, `BOX_DESKTOP_REQUIRED=0`). Ready no longer waits on X. No screenshots, no voxl. Less RAM. Compose still lists 6901–6905 unless you override ports.
- **Volume ownership.** `box-prepare` sets owners on the bind mounts at start; a volume from an older image is moved aside, not deleted ([USERS.md](USERS.md)).
- **No registry by default.** First test builds from git. Tag `v*` to run the GHCR workflow before you scale to many VMs.
- **Provider confusion.** Compute = `linode/linode` + `LINODE_TOKEN`. Not Vultr. Not `akamai/akamai`.

---

## References

- Product: [`README.md`](../README.md), [`docs/ARCHITECTURE.md`](ARCHITECTURE.md), [`docs/API.md`](API.md), [`docs/STARTUP.md`](STARTUP.md)
- Compose / image: [`docker-compose.yml`](../docker-compose.yml), [`docker/Dockerfile`](../docker/Dockerfile)
- Akamai Cloud Manager: [https://cloud.linode.com/](https://cloud.linode.com/)
- API tokens: [https://techdocs.akamai.com/cloud-computing/docs/manage-personal-access-tokens](https://techdocs.akamai.com/cloud-computing/docs/manage-personal-access-tokens)
- Cloud Firewalls: [https://techdocs.akamai.com/cloud-computing/docs/cloud-firewall](https://techdocs.akamai.com/cloud-computing/docs/cloud-firewall)
- Metadata / user-data: [https://techdocs.akamai.com/cloud-computing/docs/add-user-data-when-deploying-a-compute-instance](https://techdocs.akamai.com/cloud-computing/docs/add-user-data-when-deploying-a-compute-instance)
- Provider: [https://registry.terraform.io/providers/linode/linode/latest/docs](https://registry.terraform.io/providers/linode/linode/latest/docs)
- Pricing (confirm live): [https://www.linode.com/pricing/](https://www.linode.com/pricing/)
