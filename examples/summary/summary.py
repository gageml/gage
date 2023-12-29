import json
import os

filename = os.getenv("SUMMARY") or "summary.json"
assert filename, "SUMMARY cannot be empty"

type = "example"
fake_speed = 0.1

ext = os.path.splitext(filename)[1]
assert ext in [".json", ".toml"], filename

print(f"Writing summary to {filename}")
with open(filename, "w") as f:
    if ext == ".json":
        json.dump(
            {
                "attributes": {"type": type},
                "metrics": {"speed": fake_speed},
            },
            f,
            indent=2,
            sort_keys=True,
        )
    elif ext == ".toml":
        f.write(
            f"""[attributes]
type = "{type}"

[metrics]
speed = {fake_speed}
"""
        )
    else:
        assert False, filename
