#!/usr/bin/env bash
# Uploads buwiz-computer.yaml to an S3 bucket you own, and prints the one-click "Launch Stack" link.
# It creates no bucket and calls no AWS API but S3's PutObject. Usage:
#   bash deploy/aws/cloudformation/publish.sh <bucket> [region]
# The bucket is yours: CloudFormation reads the template as the person who opens the link, so that
# person must be able to read the object. Whether we host a public copy is the owner's decision.
set -euo pipefail

usage() {
  echo "usage: $0 <bucket> [region]" >&2
  exit 2
}

[[ $# -ge 1 && $# -le 2 ]] || usage
bucket="$1"
region="${2:-${AWS_REGION:-ap-southeast-1}}"

if [[ ! "${bucket}" =~ ^[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$ ]]; then
  echo "not a bucket name: ${bucket}" >&2
  exit 2
fi
if [[ ! "${region}" =~ ^[a-z]{2}(-[a-z]+)+-[0-9]+$ ]]; then
  echo "not an AWS region: ${region}" >&2
  exit 2
fi
command -v aws >/dev/null 2>&1 || { echo "needs the AWS CLI (brew install awscli)" >&2; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "needs python3, to encode the URL" >&2; exit 1; }

ROOT="$(cd "$(dirname "$0")" && pwd)"
TEMPLATE="${ROOT}/buwiz-computer.yaml"
KEY="buwiz-computer/buwiz-computer.yaml"

aws s3 cp --only-show-errors --region "${region}" \
  --content-type application/x-yaml "${TEMPLATE}" "s3://${bucket}/${KEY}"

url="https://${bucket}.s3.${region}.amazonaws.com/${KEY}"
encoded="$(python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1], safe=""))' "${url}")"

echo "uploaded: s3://${bucket}/${KEY}"
echo "template URL: ${url}"
echo "quick-create (Launch Stack): https://console.aws.amazon.com/cloudformation/home?region=${region}#/stacks/quickcreate?templateURL=${encoded}&stackName=buwiz-computer"
echo "The link works only for someone who can read that object. Parameters: AllowedSshCidr is required."
