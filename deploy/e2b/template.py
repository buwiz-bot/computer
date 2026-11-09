#!/usr/bin/env python3
"""Builds the Buwiz Bot computer template in the E2B team that owns E2B_API_KEY.

Run by build.sh, which sets up the SDK. The SDK reads E2B_API_KEY itself; this script never prints it.
Prints the alias (what BUWIZ_E2B_TEMPLATE takes) and the template id. Rebuilding with the same alias
makes a new build of it.

e2b.Dockerfile is the box image plus what E2B's sandbox needs: the directory the secrets are staged
in, owned by box, and the non-secret settings of docker-compose.yml. It has no comments on purpose:
the SDK's Dockerfile reader prints one "Unsupported instruction" line for each. The SDK keeps FROM,
USER, RUN, ENV and WORKDIR, and reads ENV one per line. No secret goes in the Dockerfile, a build
argument or the environment: the box reads /run/buwiz/token and /run/buwiz/viewer and unlinks them.
"""

import os
import sys
from pathlib import Path

from e2b import Template, default_build_logger, wait_for_timeout

HERE = Path(__file__).resolve().parent

ALIAS = os.environ.get("BUWIZ_E2B_ALIAS", "buwiz-computer")

# The start command runs once, at the end of the build, and E2B snapshots the sandbox with it
# running. It waits for the `go` file that the broker uploads after token and viewer, then runs the
# box exactly as the image's ENTRYPOINT does (tini, then box-keeper).
START_CMD = "sh -c 'until [ -f /run/buwiz/go ]; do sleep 0.2; done; exec /usr/bin/tini -- /usr/local/bin/box-keeper'"

# The largest a Hobby team may give a sandbox: 8 vCPU and 8 GiB. Pro allows more on request.
CPU_COUNT = int(os.environ.get("BUWIZ_E2B_CPU", "8"))
MEMORY_MB = int(os.environ.get("BUWIZ_E2B_MEMORY_MB", "8192"))


def main() -> int:
    if not os.environ.get("E2B_API_KEY"):
        print("E2B_API_KEY is not set: nothing was built.", file=sys.stderr)
        return 2
    if MEMORY_MB % 2 or MEMORY_MB < 1024:
        print("BUWIZ_E2B_MEMORY_MB must be an even number of MiB, at least 1024.", file=sys.stderr)
        return 2
    if CPU_COUNT < 1:
        print("BUWIZ_E2B_CPU must be at least 1.", file=sys.stderr)
        return 2

    template = (
        Template(file_context_path=str(HERE))
        .from_dockerfile((HERE / "e2b.Dockerfile").read_text())
        .set_start_cmd(START_CMD, wait_for_timeout(5_000))
    )
    info = Template.build(
        template,
        ALIAS,
        cpu_count=CPU_COUNT,
        memory_mb=MEMORY_MB,
        on_build_logs=default_build_logger(),
    )
    print(f"template alias: {info.alias}")
    print(f"template id:    {info.template_id}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
