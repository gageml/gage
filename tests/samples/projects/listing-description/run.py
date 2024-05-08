import json

i = 123
s = "hello"
b = True

with open("summary.json", "w") as f:
    json.dump({"metrics": {"z": 1.123}}, f)
