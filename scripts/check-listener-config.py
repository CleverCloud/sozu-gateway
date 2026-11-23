#!/usr/bin/env python3
"""Check the static listeners in a rendered Sōzu ConfigMap, read from stdin.

With no argument, the ci/multiple-listeners.yaml fixture: multiple listeners
preserve TOML table boundaries and every listener option lands where it
belongs. With `--h2c on` or `--h2c off`, on any exposure table, only where
`h2c_prior_knowledge` lands: on every HTTP listener and no HTTPS one, or
nowhere in the file at all — absent, never `false`, because a Sōzu older than
2.3.4 refuses to start with the key in its config whatever its value.
"""
import argparse
import sys
import textwrap
import tomllib

parser = argparse.ArgumentParser()
parser.add_argument("--h2c", choices=["on", "off"])
args = parser.parse_args()

raw = textwrap.dedent(sys.stdin.read().split("config.toml: |\n", 1)[1])
config = tomllib.loads(raw)
listeners = config["listeners"]


def check_h2c(enabled):
    if not enabled:
        assert "h2c_prior_knowledge" not in raw, "h2c_prior_knowledge rendered while off"
        return
    assert any(listener["protocol"] == "http" for listener in listeners)
    for listener in listeners:
        if listener["protocol"] == "http":
            assert listener.get("h2c_prior_knowledge") is True, listener["address"]
        else:
            assert "h2c_prior_knowledge" not in listener, listener["address"]


if args.h2c:
    check_h2c(args.h2c == "on")
    sys.exit(0)

assert [(item["protocol"], item["address"]) for item in listeners] == [
    ("http", "0.0.0.0:8080"),
    ("https", "0.0.0.0:8443"),
    ("http", "0.0.0.0:5300"),
    ("https", "0.0.0.0:9443"),
]
assert config["front_timeout"] == 45
check_h2c(True)
for listener in listeners:
    assert listener["sozu_id_header"] == "x-proxy-id"
    assert listener["send_x_real_ip"] is True
    assert listener["elide_x_real_ip"] is True
    if listener["protocol"] == "https":
        assert listener["tls_versions"] == ["TLS_V12", "TLS_V13"]
        assert listener["strict_sni_binding"] is False
        assert listener["hsts"] == {"enabled": True, "max_age": 86400}
    else:
        assert "tls_versions" not in listener
        assert "strict_sni_binding" not in listener
        assert "hsts" not in listener
