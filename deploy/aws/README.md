# deploy/aws: where Buwiz Bot's EC2 computers launch

This OpenTofu module makes everything a Buwiz Bot computer on AWS needs except the computer itself.
The broker on your Mac launches one instance per computer from the launch template made here. It
hibernates the instance when the computer has been idle for 30 minutes, starts it again on the next
use, and terminates it when you delete the computer. OpenTofu never creates an instance.

The defaults: region `ap-southeast-1` (Singapore), `m7i.large` (2 vCPU, 8 GiB), Ubuntu 24.04, an
encrypted 60 GiB gp3 root volume, and hibernation on.

## What it creates

| Resource | What for |
| --- | --- |
| VPC `10.73.0.0/16`, an internet gateway, one public subnet and a route table | Where instances land. Not made if you set `subnet_id`. |
| Security group `buwiz-computer` | Inbound TCP 22 from `allowed_ssh_cidrs` only, and nothing else. All outbound, because the box browses. The box's ports (1337, 1340, 6901–6905) are bound to the instance's loopback, and the broker reaches them through SSH. |
| Launch template `buwiz-computer` | Canonical's latest Ubuntu 24.04 AMI for the type's architecture, `m7i.large`, an encrypted gp3 root volume deleted with the instance, `hibernation_options.configured = true`, IMDSv2 required with a hop limit of 1 (a container can't get a token), instance tags in metadata, a public IPv4 address, and the user data in `user-data.yaml`. |
| Only with `enable_ssm = true`: role and instance profile `buwiz-computer-instance` | SSM Session Manager, and nothing more. By default instances have no role at all. |
| Only with `local_zone`: the zone group's opt-in | See [Manila](#the-manila-local-zone). |

`user-data.yaml` installs Docker (`docker.io`, signed by Ubuntu) and the hibernation agent, prints
the SSH host keys to the console, and pulls the box image. It contains no secret: every process on
the instance can read its user data, and so can anyone who can read cloud-init's logs. At launch
the broker adds one more part, the computer's SSH **public** key.

`broker-policy.json` is the managed policy of the broker's IAM user, `buwiz-computer-broker`. It
allows describing and reading the console in the region. It allows RunInstances only with
`buwiz:managed=true` set at launch on the instance and its volume, and CreateTags only as part of
RunInstances. It allows start, stop, terminate and reboot only on instances tagged
`buwiz:managed=true`, and PassRole only on `buwiz-computer*` roles. The broker uses nothing else:
it imports no key pairs and adds no tags after launch.

The checked-in file has `<ACCOUNT_ID>` and `<REGION>` where the account and region go, because each
client runs this in their own AWS account. `tofu output -raw broker_policy_json` prints it filled in
for the account you apply as (`aws_caller_identity`) and the module's `region`. Give that to the
broker's IAM user, for example:

```sh
AWS_PROFILE=buwiz tofu output -raw broker_policy_json > /tmp/broker-policy.json
AWS_PROFILE=buwiz aws iam create-policy --policy-name buwiz-computer-broker \
  --policy-document file:///tmp/broker-policy.json
AWS_PROFILE=buwiz aws iam attach-user-policy --user-name buwiz-computer-broker \
  --policy-arn "arn:aws:iam::$(AWS_PROFILE=buwiz aws sts get-caller-identity --query Account --output text):policy/buwiz-computer-broker"
```

## How the broker uses it

1. **Launch.** The broker makes an ed25519 key pair for the computer, kept in that computer's
   folder in the broker's data dir as a 0600 file. It runs one instance from the template, with
   the template's own user data plus the public key, and tags `Name=buwiz-computer-<id>`,
   `buwiz:managed=true` and `buwiz:computer-id=<id>` on the instance and its volume.
2. **Trust.** It reads the instance's console through the EC2 API (`GetConsoleOutput`) until
   cloud-init has printed the SSH host keys. It pins the ed25519 key, then logs in with `ssh`, which
   refuses any other key. No secret is sent before that.
3. **Start.** As root over SSH it puts up a wall, then starts the box. The wall is netfilter rules
   on the instance itself: the box's bridge can't reach private, link-local or reserved addresses
   (the VPC, the metadata service), only Amazon's DNS resolver on port 53, and it can't open
   connections to the instance. The box is the same image and hardening as on a Mac (capabilities
   dropped, no new privileges, pids and memory limits, loopback-only ports, volumes on the root
   EBS). The box's token and viewer password reach the start on stdin, are staged in a root-only
   tmpfs folder, and are removed once the box answers.
4. **Use.** One SSH master forwards the box's ports to loopback ports on your Mac.
5. **Sleep.** Light sleep is `StopInstances` with `Hibernate=true`: memory goes to the encrypted
   root volume, and the next start brings back the same desktop and browser. An instance that
   can't hibernate is stopped instead, and the broker's log says so. Deep sleep stops the box
   first, then stops the instance without hibernating.
6. **Delete.** `TerminateInstances` removes the instance, and its root volume, which holds the
   files, goes with it. A computer whose record is lost is found or swept only by both of its tags.

## Try it

You need OpenTofu (`brew install opentofu`) and the two AWS profiles on this Mac:

- `buwiz`: the IAM user `buwiz-admin`, signed in with `aws login`. It's an administrator with a
  short-lived session. This is the profile that applies the module, and the broker never uses it.
- `buwiz-broker`: the IAM user `buwiz-computer-broker`, with a static access key in
  `~/.aws/credentials`, limited by the policy `broker_policy_json` prints. This is the profile the broker signs with.

```sh
cd deploy/aws
aws login --profile buwiz           # the session runs out: log in again when apply says so
AWS_PROFILE=buwiz tofu init
AWS_PROFILE=buwiz tofu plan         # read it: it creates no instance
AWS_PROFILE=buwiz tofu apply
tofu output broker_env
tofu output -raw broker_policy_json  # the broker user's policy, for this account
```

If `allowed_ssh_cidrs` is left empty, SSH is open to the public address you apply from, as
`checkip.amazonaws.com` sees it. If that address changes (home connections, mobile networks, CGNAT),
the broker can't reach its computers until you apply again, or until you set the CIDRs:
`tofu apply -var 'allowed_ssh_cidrs=["203.0.113.0/24"]'`. Don't use `0.0.0.0/0`: SSH takes only the
computer's own key, but nothing needs to reach port 22 from everywhere.

Then start the broker with what `broker_env` printed. Settings are environment variables for now.
The broker reads them at start, so set them where the broker starts. For an app opened from Finder,
use `launchctl setenv NAME value` for each one, then quit and reopen Buwiz Bot. For a test bench,
set them in the shell that runs `scripts/buwiz-test`.

| Variable | Default | |
| --- | --- | --- |
| `BUWIZ_COMPUTER_PROVIDER` | `docker` | `ec2` makes new computers on EC2. Computers made before keep their provider. |
| `BUWIZ_EC2_REGION` | `ap-southeast-1` | Must match `region` here. |
| `BUWIZ_EC2_LAUNCH_TEMPLATE` | `buwiz-computer` | Its name or `lt-…` id. |
| `BUWIZ_EC2_IMAGE` | `ghcr.io/buwiz-bot/computer:latest` | Should match `image:image_tag` here, so the first boot's pull is the one used. |
| `BUWIZ_EC2_USER` | `ubuntu` | The AMI's login. |
| `BUWIZ_EC2_PROFILE` | `buwiz-broker` | The profile in `~/.aws/credentials` (or `~/.aws/config`). `AWS_PROFILE` is not read, so a shell pointed at the admin profile doesn't hand it to the broker. `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN`, when set, come first. |

The first wake of a new computer takes several minutes: cloud-init installs Docker, pulls the image
and prints the host keys before anything can log in. After a hibernation, a wake takes about as long
as the instance takes to start.

## Cost

This is a rough guide. Check [EC2 pricing](https://aws.amazon.com/ec2/pricing/on-demand/) for
`ap-southeast-1` before you rely on it.

- **Awake:** `m7i.large` on demand, around US$0.12–0.13 an hour, plus US$0.005 an hour for its public
  IPv4 address. Idle for 30 minutes, it hibernates.
- **Asleep (hibernated or stopped):** no instance charge and no address charge. The root volume is
  still billed: gp3 is about US$0.096 per GiB-month in Singapore, so a 60 GiB computer is about
  US$5.80 a month for as long as it exists.
- **Traffic:** data out to the internet is billed per GB. Watching a screen streams video through
  SSH.
- **Hibernated for more than 60 days** is past what AWS supports: wake the computer at least once
  in that time.
- What this module makes costs nothing while it sits there (VPC, subnet, security group, launch
  template). Only instances and their volumes cost money.

## Destroy

The broker's instances aren't in OpenTofu's state, so delete the computers first. Delete each EC2
computer's Bot in Buwiz Bot (that terminates its instance), or terminate them yourself:

```sh
AWS_PROFILE=buwiz aws ec2 describe-instances --region ap-southeast-1 \
  --filters Name=tag:buwiz:managed,Values=true Name=instance-state-name,Values=pending,running,stopping,stopped \
  --query 'Reservations[].Instances[].InstanceId' --output text
AWS_PROFILE=buwiz aws ec2 terminate-instances --region ap-southeast-1 --instance-ids <ids>
```

Then `AWS_PROFILE=buwiz tofu destroy`. The security group and subnet can't be deleted while an
instance is still using them.

## The Manila Local Zone

`-var local_zone=ap-southeast-1-mnl-1a` opts in the zone group (`ap-southeast-1-mnl-1`) and puts the
made subnet there, which is closer to the Philippines. **Unverified:** Local Zones offer few instance
types and may not offer gp3, and whether `m7i` runs there, or any type there can hibernate, hasn't
been checked. Set `instance_type` to one the zone lists (`aws ec2 describe-instance-type-offerings
--location-type availability-zone --filters Name=location,Values=ap-southeast-1-mnl-1a`). If
hibernation isn't possible there, set `hibernation=false`; sleep is then a plain stop.

## Not verified without a live apply

- The hibernation agent and swap on Canonical's Ubuntu 24.04 AMI (the user data installs
  `ec2-hibinit-agent` in case the AMI hasn't got it), and the time after launch before an instance
  can hibernate. Until then the broker falls back to a stop.
- That cloud-init on this AMI prints the key block the broker reads (the user data asks it to).
- That RunInstances with the default `aws/ebs` key needs no KMS permission beyond the policy. A
  `kms_key_id` of your own would need the broker's user to be allowed to use that key.
- Docker's DNS with `--dns 169.254.169.253` behind the wall's rules.
