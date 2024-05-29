import json

x = 1

summary = {
    "metrics": {
        "y": x + 1,
    },
    "attributes": {"z": "x" if x else "not x"},
}

print(json.dumps(summary))
