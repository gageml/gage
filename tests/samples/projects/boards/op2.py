import json

x = 1

x_type = "red" if x > 1 else "blue"
x_speed = 1.123 if x > 1 else 0.987

summary = {
    "run": {
        "label": f"x={x}"
    },
    "attributes": {
        "type": x_type
    },
    "metrics": {
        "speed": x_speed
    }
}

with open("summary.json", "w") as f:
    json.dump(summary, f)
