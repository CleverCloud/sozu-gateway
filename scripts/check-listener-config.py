#!/usr/bin/env python3
"""Check that multiple static listeners preserve TOML table boundaries."""
import sys
import textwrap
import tomllib

config = tomllib.loads(textwrap.dedent(sys.stdin.read().split("config.toml: |\n", 1)[1]))
listeners = config["listeners"]
assert [(item["protocol"], item["address"]) for item in listeners] == [
    ("http", "0.0.0.0:8080"),
    ("https", "0.0.0.0:8443"),
    ("http", "0.0.0.0:5300"),
    ("https", "0.0.0.0:9443"),
]
assert config["front_timeout"] == 45
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
