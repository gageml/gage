import json

summary = {
    "attributes": {
        "color": {
            "value": "red",
            "links": ["/color", "/attr"]
        }
    },
    "metrics": {
        "height": 1.2,
        "width": {
            "value": 2.4,
            "comment": "2x height"
        }

    }
}

with open("summary.json", "w") as f:
    json.dump(summary, f, indent=2, sort_keys=True)
