#!/usr/bin/env bash
# Fails when deploy/aws (OpenTofu) and deploy/aws/cloudformation/buwiz-computer.yaml disagree on what
# a client gets: the defaults, the launch template's settings, the security group, the role, the
# broker's policy, the user data and the broker's env lines. Stdlib Python, no AWS calls.
# Known differences, not checked: tofu's region default (CloudFormation uses the stack's region),
# tofu's subnet_id / local_zone / availability_zone inputs and existing-subnet mode (CloudFormation
# always makes the VPC), and the broker's IAM user (CloudFormation makes it; tofu leaves it to you).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"

python3 - "${ROOT}" <<'PY'
import json
import re
import sys
from pathlib import Path

root = Path(sys.argv[1])
tofu = root
cfn = root / "cloudformation" / "buwiz-computer.yaml"
failures = []


def check(name, ok, detail=""):
    print(("ok    " if ok else "DRIFT ") + name + ("" if ok else f": {detail}"))
    if not ok:
        failures.append(name)


def read(path):
    return path.read_text()


def tf_default(text, var):
    m = re.search(r'variable "%s" \{' % re.escape(var), text)
    if not m:
        return None
    end = text.find("\n}\n", m.end())
    block = text[m.end():end]
    d = re.search(r"^\s*default\s*=\s*(.+?)\s*$", block, re.M)
    return d.group(1).strip().strip('"') if d else None


def tf_block(text, header):
    m = re.search(re.escape(header) + r"\s*\{", text)
    if not m:
        return ""
    depth, i = 1, m.end()
    while depth and i < len(text):
        depth += {"{": 1, "}": -1}.get(text[i], 0)
        i += 1
    return text[m.end():i - 1]


def attr(block, key):
    m = re.search(r"^\s*%s\s*=\s*(.+?)\s*$" % re.escape(key), block, re.M)
    return m.group(1).strip().strip('"') if m else None


def cfn_value(text, key, indent_min=0):
    m = re.search(r"^\s*%s:\s*(.+?)\s*$" % re.escape(key), text, re.M)
    return m.group(1).strip().strip('"') if m else None


def cfn_block(text, marker):
    """The lines of a `key: !Sub |` block: every following line indented deeper than the key."""
    lines = text.splitlines()
    for i, line in enumerate(lines):
        if line.strip() == marker:
            indent = len(line) - len(line.lstrip())
            body = []
            for nxt in lines[i + 1:]:
                if nxt.strip() == "":
                    body.append("")
                    continue
                if len(nxt) - len(nxt.lstrip()) <= indent:
                    break
                body.append(nxt)
            low = min(len(b) - len(b.lstrip()) for b in body if b.strip())
            return "\n".join(b[low:] if b.strip() else "" for b in body).rstrip() + "\n"
    return None


tf_vars = read(tofu / "variables.tf")
tf_main = read(tofu / "main.tf")
tf_outputs = read(tofu / "outputs.tf")
cf = read(cfn)

# ---- defaults ----
check("instance type default", re.search(r"InstanceType:\n\s+Type: String\n\s+Default: (\S+)", cf).group(1) == tf_default(tf_vars, "instance_type"))
check("root volume default", re.search(r"RootVolumeGiB:\n\s+Type: Number\n\s+Default: (\d+)", cf).group(1) == tf_default(tf_vars, "root_volume_gib"))
check("root volume minimum", re.search(r"RootVolumeGiB:[\s\S]*?MinValue: (\d+)", cf).group(1) == "30" and
      re.search(r"var\.root_volume_gib >= (\d+)", tf_vars).group(1) == "30")
check("hibernation default", tf_default(tf_vars, "hibernation") == "true")
check("hibernation configured in the launch template", re.search(r"HibernationOptions:\s*\n\s+Configured: true", cf) is not None)
check("VPC CIDR default", re.search(r"VpcCidr:\n\s+Type: String\n\s+Default: (\S+)", cf).group(1) == tf_default(tf_vars, "vpc_cidr"))
check("subnet is cidrsubnet(vpc, 8, 1)", "cidrsubnet(var.vpc_cidr, 8, 1)" in tf_main and "!Select [1, !Cidr [!GetAtt Vpc.CidrBlock, 256, 8]]" in cf)
check("image default", (re.search(r"BoxImage:\n\s+Type: String\n\s+Default: (\S+)", cf).group(1) ==
      f"{tf_default(tf_vars, 'image')}:{tf_default(tf_vars, 'image_tag')}"))
check("SSM off by default", tf_default(tf_vars, "enable_ssm") == "false" and re.search(r'EnableSsm:\n\s+Type: String\n\s+Default: "false"', cf) is not None)
check("KMS key default is the account default", tf_default(tf_vars, "kms_key_id") in (None, "null") and re.search(r'KmsKeyId:\n\s+Type: String\n\s+Default: ""', cf) is not None)

# ---- launch template ----
meta = tf_block(tf_main, "metadata_options")
for tf_key, cf_key, want in [
    ("http_endpoint", "HttpEndpoint", "enabled"),
    ("http_tokens", "HttpTokens", "required"),
    ("http_put_response_hop_limit", "HttpPutResponseHopLimit", "1"),
    ("instance_metadata_tags", "InstanceMetadataTags", "enabled"),
]:
    check(f"IMDS {tf_key}", attr(meta, tf_key) == want and cfn_value(cf, cf_key) == want,
          f"tofu={attr(meta, tf_key)} cfn={cfn_value(cf, cf_key)}")

ebs = tf_block(tf_main, "ebs")
check("root volume gp3, encrypted, deleted with the instance",
      attr(ebs, "volume_type") == "gp3" and attr(ebs, "encrypted") == "true" and attr(ebs, "delete_on_termination") == "true"
      and cfn_value(cf, "VolumeType") == "gp3" and cfn_value(cf, "Encrypted") == "true" and cfn_value(cf, "DeleteOnTermination") == "true")
check("root volume size is the variable", attr(ebs, "volume_size") == "var.root_volume_gib" and "VolumeSize: !Ref RootVolumeGiB" in cf)
check("launch template name", tf_default(tf_vars, "name") == "buwiz-computer" and "LaunchTemplateName: buwiz-computer" in cf)
check("instance profile / role name", f"\"${{var.name}}-instance\"" in tf_main and "RoleName: buwiz-computer-instance" in cf
      and "InstanceProfileName: buwiz-computer-instance" in cf)
check("public IPv4 and a single public subnet", "associate_public_ip_address = true" in tf_main and "AssociatePublicIpAddress: true" in cf)

# ---- security group ----
check("SSH on 22 only, from the allowed CIDR", "from_port         = 22" in tf_main and "FromPort: 22" in cf and "ToPort: 22" in cf
      and "CidrIp: !Ref AllowedSshCidr" in cf)
check("egress is all, to the internet", '"0.0.0.0/0"' in tf_main and "CidrIp: 0.0.0.0/0" in cf and 'IpProtocol: "-1"' in cf)
ssh_param = re.search(r"  AllowedSshCidr:\n([\s\S]*?)\n  InstanceType:", cf).group(1)
check("no default for the SSH CIDR", "Default:" not in ssh_param)

# ---- user data ----
tf_user = read(tofu / "user-data.yaml")
cf_user = cfn_block(cf, "Fn::Base64: !Sub |")
check("user data present in the template", cf_user is not None)
if cf_user is not None:
    cf_user_norm = cf_user.replace("${BoxImage}", "${image}")
    tf_user_norm = tf_user.rstrip() + "\n"
    check("user data is user-data.yaml", cf_user_norm == tf_user_norm,
          "the CloudFormation copy of user-data.yaml differs from deploy/aws/user-data.yaml")

# ---- broker policy ----
tf_policy = json.loads(read(tofu / "broker-policy.json"))
cf_policy_text = cfn_block(cf, "PolicyDocument: !Sub |")
if cf_policy_text is None:
    check("broker policy in the template", False, "no PolicyDocument !Sub block")
else:
    cf_policy_text = cf_policy_text.replace("${AWS::AccountId}", "<ACCOUNT_ID>").replace("${AWS::Region}", "<REGION>")
    check("broker policy is broker-policy.json", json.loads(cf_policy_text) == tf_policy,
          "statements differ between broker-policy.json and the template")

cf_out_policy = cf.split("  BrokerPolicyJson:", 1)[1]
cf_out_policy = cfn_block(cf_out_policy, "Value: !Sub |")
if cf_out_policy is None:
    check("broker policy as an output", False, "no BrokerPolicyJson output")
else:
    check("broker policy output is broker-policy.json", json.loads(cf_out_policy.replace("${AWS::AccountId}", "<ACCOUNT_ID>").replace("${AWS::Region}", "<REGION>")) == tf_policy,
          "the BrokerPolicyJson output differs from broker-policy.json")

# ---- the broker's env lines ----
tf_env = re.search(r"<<-EOT\n(.*?)\n\s*EOT", tf_outputs, re.S).group(1)
tf_keys = [l.strip().split("=", 1)[0] for l in tf_env.splitlines() if l.strip()]
cf_env_block = cfn_block(cf, "Value: !Sub |")
cf_keys = [l.strip().split("=", 1)[0] for l in (cf_env_block or "").splitlines() if l.strip()]
check("broker env names", tf_keys == cf_keys, f"tofu={tf_keys} cfn={cf_keys}")
check("broker env provider and profile", "BUWIZ_COMPUTER_PROVIDER=ec2" in tf_env and "BUWIZ_COMPUTER_PROVIDER=ec2" in (cf_env_block or "")
      and "BUWIZ_EC2_PROFILE=buwiz-broker" in tf_env and "BUWIZ_EC2_PROFILE=buwiz-broker" in (cf_env_block or ""))

print()
if failures:
    print(f"{len(failures)} check(s) drifted: change deploy/aws and deploy/aws/cloudformation/buwiz-computer.yaml together.")
    sys.exit(1)
print("parity OK")
PY
