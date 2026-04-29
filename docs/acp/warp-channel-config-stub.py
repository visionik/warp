#!/usr/bin/env python3
"""
Stub replacement for the private `warp-channel-config` binary.

The real binary lives in a private Warp repository and is unavailable to
external contributors.  This stub emits the minimum valid ChannelConfig JSON
so that local debug builds (`cargo build --bin warp`) can start.

Usage — put this script on PATH as 'warp-channel-config':

  # Option A: symlink
  chmod +x docs/acp/warp-channel-config-stub.py
  ln -sf "$(pwd)/docs/acp/warp-channel-config-stub.py" /usr/local/bin/warp-channel-config

  # Option B: prefix PATH per invocation
  PATH="$(pwd)/docs/acp:$PATH" ./target/debug/warp agent run ...

The script must be executable and named exactly 'warp-channel-config' on PATH,
OR the PATH approach above renames it via the directory entry.
For Option B, rename the script or create a wrapper:

  cp docs/acp/warp-channel-config-stub.py /tmp/warp-channel-config
  chmod +x /tmp/warp-channel-config
  PATH="/tmp:$PATH" ./target/debug/warp agent run ...

Authentication (warp login) is still required for commands that call the
Warp server (e.g. `agent run`).
"""
import json
import sys

# Minimal ChannelConfig that matches warp_core::channel::config::ChannelConfig.
# All optional fields are null to disable telemetry, autoupdate, and crash
# reporting in local test runs.
config = {
    "app_id": "dev.warp.Warp-Local",
    "logfile_name": "warp-local.log",
    "server_config": {
        "server_root_url": "https://app.warp.dev",
        "rtc_server_url": "wss://rtc.app.warp.dev/graphql/v2",
        "session_sharing_server_url": None,
        "firebase_auth_api_key": "AIzaSyBdy3O3S9hrdayLJxJ7mriBR4qgUaUygAs",
    },
    "oz_config": {
        "oz_root_url": "https://oz.warp.dev",
        "workload_audience_url": None,
    },
    "telemetry_config": None,
    "autoupdate_config": None,
    "crash_reporting_config": None,
    "mcp_static_config": None,
}

print(json.dumps(config), flush=True)
sys.exit(0)
