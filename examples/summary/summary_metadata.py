import json

with open("summary.json", "w") as f:
    json.dump(
        {
            "metrics": {
                "foo": {
                    "value": 1.123,
                    "color": "green",
                }
            }
        },
        f,
        indent=2,
        sort_keys=True,
    )
